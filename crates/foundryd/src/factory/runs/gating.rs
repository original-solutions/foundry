//! A run while its pull request is gated.
//!
//! Ported from `src/runner/gating.ts` (the `gate` function and what it calls, not `regate`).
//! [`advance`] is one pass over a `building` run whose session has reported its pull request
//! (`building::advance` said `ReadyForGate`). A pass does at most one step of the gate and
//! returns; no pass waits on a worker. Each step is recorded in the run's ledger before the
//! thing it records is asked of GitHub or a worker, and a step already recorded is not done
//! again, so a pass cut short and repeated carries on and asks nothing twice. The lease is
//! checked, through `RunContext`'s methods, before every GitHub change and every worker call.
//!
//! What a pass does, in order:
//!
//! 1. A closed issue, a run finished or not building, a stop asked for and a run already in
//!    `pr-open` are settled as the runner settles them (`closed_issue`, `abandon`,
//!    `end_own_merge`, `go_stuck` with `stop-requested`). A run in `pr-open` is feedback's:
//!    the pass writes nothing, except that it finishes the end of a pass an earlier call left
//!    owing (the build session ended, the build workspace archived: see 7).
//! 2. A send owed from an earlier pass (`gate-sending` with no `gate_sent_back`) is finished
//!    first, under the turn it was begun for, whatever the session or the head is doing now.
//!    Then a session still at work (starting, working, or waiting for a person), one with no
//!    turn record, one that has run no turn since a failure was sent under the newest, and a run
//!    with no pull request reported are [`GateStep::NotReady`]. A session that cannot be reached
//!    and was at rest when last seen ([`at_rest`]) is not at work: its reported pull request is
//!    gated, and a gate in flight on its machine is read, so a cut-off build machine's gate could
//!    not run as any other's. One that may be at work there is waited for, and with its pull
//!    request reported nothing interrupts it: `limits.minutes` ends the run (#367).
//!    A session the fleet does not show is stuck (`session-gone`).
//! 3. A pull request that is gone, from a fork, off the base or off the run's branches is stuck
//!    (`pr-not-bound`); one that was merged or closed before a pass is recorded ends the run
//!    (`end_run`). One merged or closed after the pass is recorded has the pass finished as it
//!    stands, with nothing marked ready or posted, and the run left to feedback and stop.
//! 4. **Place.** With no gate in flight, the pass places one while a gate slot is free
//!    (`slots.gates_free`, which this pass takes: two runs in one pass do not both take the
//!    last). With none free it is [`GateStep::Queued`] and nothing is written or asked. A
//!    pull request GitHub says conflicts with its base is a failed attempt at once, sent back
//!    with the conflict message and taking no slot. Otherwise the pass reads the pull
//!    request's head and the base branch's tip, picks a machine with `placing::place_gate`,
//!    and writes `gate-requested <attempt> <k> <head> <base-tip> <machine>` (the commit gated,
//!    recorded before the create) and then asks for the workspace
//!    `gate::request` builds from the config the context carries (the default branch's, never
//!    the worktree's). The answer is recorded as `gate-created <k> <workspace>` with a
//!    `gate_checkout` event (the time a gate runs is not the run's, `Seen::worked_ms`). A
//!    workspace the view already shows under the gate's name is not asked for again.
//! 5. **Read.** On later passes the gate's setup terminal is read from the fleet view
//!    (`Workspace::setup`, then `FleetView::terminal`). While it runs the pass is
//!    [`GateStep::Running`]. A gate that `gate::could_not_run` (the create failed, no
//!    machine qualifies, the machine unreachable for `MACHINE_GONE_AFTER`, a terminal that
//!    exited with no code) is not a failure: its slot is freed and it is tried again after
//!    `gate::backoff` passes (1, 2, 4, 8), the fifth failure being stuck. It records
//!    `gate-could-not-run <attempt> <k> <failures> <wait> <reason>`, and each pass of the wait
//!    records `gate-waiting <failures>` and is [`GateStep::BackingOff`]. It never spends
//!    `gate_retries`, and the attempt keeps its number. A gate that could not run and whose
//!    workspace exists on a machine that is not gone is archived by force before another is
//!    placed; a gone machine's (unreachable for `MACHINE_GONE_AFTER`) is left to
//!    `resume::orphans`, and its slot is free all the same. A gate created whose workspace or
//!    setup terminal the fleet does not show while its machine is online is given
//!    `UNSEEN_PASSES` passes (`gate-unseen <k>`) for the record to arrive, and then could
//!    not run. A terminal that exited is read once: the worker's tail is cut to its last
//!    64 KB and kept in the `error` of a `gate_cleanup_failed` event whose `dir` is the gate's
//!    workspace name (the one event with a text field of its own, which the ledger cuts and
//!    hides on its own, so nothing done to the tail can reach the verdict), then
//!    `gate-ended <k> <exit code>` records the verdict, a short step. Then the gate's workspace
//!    is archived by force by one rule (`archive_gate`): only `archive` is called, never a
//!    create (which would start the pull request's commands). `gate-archiving <k>`, the call,
//!    then `gate-archived <k>`, written only after a call answered Ok or P1's "is not a live
//!    workspace of this machine", never from what the view shows or does not. A recorded id
//!    is always archived by call; with none (a create that failed or got no answer may have
//!    made one) every live workspace the view shows under the gate's name is, and with none
//!    shown the gate waits the unseen grace (blocking the next gate: nothing is placed until
//!    it is archived or left), then is recorded `gate-left <k> <id or -> <why>` (fixed words)
//!    for `resume::orphans` or a person. On an exit path (stuck, stop, abandon, end) a refused
//!    or unreachable archive is recorded left at once and the run goes on, as the runner notes
//!    a leftover checkout it cannot remove; while placing gates, after `ARCHIVE_TRIES`
//!    refusals. Before `pr-open` it departs from the runner, because the issue's acceptance
//!    criterion (no workspace of the run is left) requires it: every gate on a machine that
//!    is not gone, one recorded left included, is archived first, a refusal is asked again
//!    next pass and the run does not move, and after `ARCHIVE_TRIES` refusals the run is
//!    stuck ("a gate workspace could not be archived"). A gate of a gone machine is recorded
//!    left and holds nothing; the spec has that machine archive it when heard from again. A
//!    sweep of `gate-left` workspaces on an online machine belongs to tasks 28 and 30 (a
//!    follow-up), the spec keeping a stuck run's workspaces for a person anyway.
//!
//!    The build and the gate workspaces follow one rule, the gate's with the spec's 5 minutes
//!    for an unreachable gate machine (`gate::MACHINE_GONE_AFTER`) and the build's with its
//!    15 minutes (`progress::UNREACHABLE_FOR`). "Not a live workspace" counts as archived. A
//!    machine the view shows unreachable for its time is gone: the workspace is recorded left
//!    (`gate-left <k> <id> its machine is gone`, `build-left its machine is gone`) with no
//!    call. An "unreachable" answer is not an error and not a refusal: the pass waits
//!    (`NotReady`, or `BackingOff` while placing) and asks again next pass, counted on the
//!    sleep-counting clock from the first such answer (`gate-archive-waiting <k> <generation>
//!    <ms>`, `build-archive-waiting <generation> <ms>`, as the checks wait is counted); once
//!    it has lasted the machine's time the workspace is recorded left as a gone machine's
//!    (`... its archive was unreachable`). Refusals are errors asked again, counted, and after
//!    `ARCHIVE_TRIES` end the run stuck before `pr-open` ("a gate workspace could not be
//!    archived") or from it ("the build workspace could not be archived", where `go_stuck`
//!    puts the pull request back to draft). A create with no answer waits
//!    the unseen grace (`UNSEEN_PASSES`, with the passes left in `BackingOff`) on top of the
//!    backoff before the next gate is placed. Then
//!    `gate_command` lines (those of this gate not yet on record) and
//!    `gate_finished` are written, and the verdict is acted on.
//! 6. **Act.** The pull request is read again. A head that is not the one gated is
//!    `gate-head-moved <attempt> <head>`, whichever the verdict, and the next pass gates the
//!    new head as the next attempt; a pull request merged or closed meanwhile ends the run
//!    first (a merge of the gated head records the pass before it ends). The attempt number
//!    is one more than the run's highest, and a gate's number `k` is one more than any
//!    earlier `gate-requested` of the whole run, so a gate's name is never reused. A round
//!    starts at the run's first gate and again at each return to `building` after `pr-open`
//!    (the same rule resets what this module reads of its own steps); `gate_retries` is one
//!    counter per round: the round's n-th attempt has spent `n - 1`, and an attempt past
//!    `gate_retries + 1` is not placed, whether the gate before it failed or the head moved.
//!    - **Fail.** A pull request someone took out of draft goes back to draft first
//!      (`pr_set_draft`). With a retry left the failure is sent to the session once, as the
//!      runner sends it (`gate-failing <attempt> <kind>` so a send owed is rebuilt, then
//!      `gate-sending <attempt> <turn>`, `send_once` under the key
//!      `gate-failure <attempt> <turn>`, `gate_sent_back`), and the pass is
//!      [`GateStep::SentBack`]; the session works, `building` reads its next turn and the gate
//!      comes back with the next attempt. With none left the run is stuck
//!      (`gate-retries-used-up`) with the factory's fixed line in the comment and the output
//!      in the ledger only. Putting the pull request back to draft follows `building`'s rule
//!      (a failure that got no answer from GitHub is an error asked again, a refusal with the
//!      pull request still not a draft is stuck, one merged meanwhile ends the run).
//!    - **Pass.** `gate_command` lines then `gate-passed <head> <count>` (cause `attempt`),
//!      recorded before anything is marked ready. With `required_checks`, the checks on that
//!      commit are read (`checks_read`): all passed finishes; one failed is sent back at once
//!      as a gate failure (`messages::check_failed`, key `gate-failure <attempt> <turn>`),
//!      stuck when no retry is left; pending or missing is [`GateStep::ChecksPending`] until
//!      `limits.checks_minutes` have passed since the wait began, then stuck
//!      (`checks-timed-out`, the read recorded first). A head that moved while the checks were
//!      waited for is gated again, like any moved head, instead of stuck. A recorded pass is
//!      finished only on the head it gated, with no exception: on every path the open pull
//!      request's head is compared with the gated commit first, and a head that moved is
//!      put back to draft when it is out of draft (by `building`'s rule), recorded, and gated
//!      again, spending a retry (stuck when none is left). So nothing ungated is marked ready,
//!      nor is anything left ready on a head that was not gated.
//! 7. **Finish a pass.** The pull request is marked ready first. Only then is the build
//!    session ended (`end_sessions`, recorded as `build-session-ended`), so a session is never
//!    ended while the pass can still be undone; a failure that must go back to a session
//!    already recorded ended in the round is stuck (`session-ended`) and not sent. The end of
//!    the session follows the build workspace's rule of 5: on a gone build machine, or after
//!    `progress::UNREACHABLE_FOR` of "unreachable" answers on the sleep-counting clock
//!    (`build-session-waiting <generation> <ms>`), it is recorded `build-session-left <why>`
//!    with no end, and on an exit path (stuck, stop, an ended run) an "unreachable" answer is
//!    recorded left at once. Then the
//!    results are posted once, the label moves to `pr-open` and the status comment is
//!    written (`apply_actions` of `pr-ready`, so `state_changed` is last); then the build
//!    workspace is archived by force (`build-archiving`, the call, `build-archived`), after
//!    the label, because a branch already on `origin` is all that is left to keep and an
//!    archive before it would leave a `building` run with no workspace if a write failed. The
//!    pass is [`GateStep::Passed`] once all of it is done, the build workspace archived or
//!    left (see 5); a pass cut short, failing or waiting finishes it on the next call, which
//!    asks for no end of sessions, ready, comment or label again (`gate-ready-marked
//!    <attempt>` notes the ready mark). A pull request merged on the gated head while the gate
//!    ran has its pass recorded and finished and the run ended in that pass; when the finish
//!    is cut short or waits, the next pass finishes the recorded pass and leaves the run in
//!    `pr-open`, and ending it as merged is the feedback step's (task 28), as for any merge
//!    after a recorded pass: the runner's two steps. A `pr-open`
//!    run whose pass is finished is answered from the ledger `advance` read, with nothing more
//!    read or asked.
//!
//! The text sent to the session is `messages::gate_failed` over the tail, and the stuck
//! comment is `messages::stuck`, as the other steps do. The public `Reason:` of a stuck
//! comment holds fixed factory words; what a worker or the placement said goes in the stuck
//! detail, in the ledger only.
//!
//! The wait for `required_checks` is counted on the sleep-counting clock (`ctx.clock`), as
//! every wait the factory measures is, and not on the wall clock (`limits.minutes`, which the
//! spec takes from the events' timestamps, is the one exception, and `building` keeps it). No
//! reading of that clock survives a coordinator change, so the start is recorded with the
//! lease generation it was read under: `checks-waiting <generation> <ms>`, written when the
//! wait begins. A later pass counts `now - recorded` only when the generation is the same and
//! `now` is not below the recorded reading; otherwise (a takeover, or a reboot that reset the
//! clock) it records a new start and the wait begins again. A takeover can therefore lengthen
//! the wait, and never ends it early. The step is read from the round's steps and cleared by
//! a new `gate-passed`.
//!
//! The placement settings (`SharedSettings`) are a parameter: `RunContext` holds none, and
//! `placing::place_gate` needs them.
//!
//! Where it differs from the runner:
//!
//! - The gate runs in a workspace of a machine, not in a local checkout under a lock: a gate
//!   in flight is the steps above, the fleet view and the gate slot, where the runner's was a
//!   lock file, a checkout directory and `FactoryGate.run`. The runner's `running` answer is
//!   [`GateStep::Running`]; its `queued`, [`GateStep::Queued`] (the caller's `Slots`, not a
//!   count of lock files). Foundry's gate checks out the head alone and does not merge the
//!   base, so a conflict is only what GitHub says (`mergeable`), with no paths.
//! - The gate's results come from the setup terminal's exit code and tail, not from command
//!   lines the gate wrote: a passed gate has one `gate_command` line per configured command
//!   with exit code 0 and no duration (so the results comment's table shows exit codes only:
//!   `gate::gate_results_comment` always has a time column, and it reads `0.0 s`), and a
//!   failed one a single line for the command `gate::result` names, with
//!   the terminal's exit code. When the tail names none (the pull request's code can erase
//!   the setup's own line), nothing is invented: the failure is the gate's commands as a
//!   whole, written `provision and gate commands joined with " && "`, and the output is the
//!   whole tail, as `gate::result` gives it.
//! - A head that moved is gated again and spends a retry (spec); the runner's gate left it for
//!   the next call under the same attempt, and its checks wait made it stuck
//!   (`checks-head-moved`). [`HEAD_KEPT_MOVING`] is the stop when none is left.
//! - A gate that could not run backs off by passes and never counts as an attempt; the runner
//!   had no such failure of its own.
//! - A failure sent back, and a stuck comment, say [`CONFIG_DIFFERS`] when `.dw/config.yml` at
//!   the head differs from the default branch's (`gate::config_differs`).
//! - The session is ended and its workspace archived by force at the pass, where the runner
//!   archived the thread and removed its worktree and left a `worktree_removed` line: Foundry
//!   writes no such line, and a build workspace that cannot be archived is asked again next
//!   pass, with the label already moved, by the rule of 5 (left when its machine is gone,
//!   stuck after `ARCHIVE_TRIES` refusals). Every gate workspace of the run is archived by
//!   force as well, and a run that ends, is abandoned or goes stuck archives its gate in
//!   flight (recorded first).
//! - A turn number stands where the runner's run ordinal did, as in `building`: a failure is
//!   sent under the prompt count the turn raised (the shared `send_once` key and count).
//! - The pull request is bound by its branch and base here with the same words `building`
//!   uses for its binding (`the session reported PR #12 but ...`); `building`'s own check is
//!   private, so this module repeats it.
//!
//! Runner cases with no counterpart here:
//!
//! - `regate`, `last_pass`, `unchangedSince`, the relaunch of a failed regate and every case
//!   of the `regate:` groups (a `pr-open` run is the feedback step's, task 28, and a feedback
//!   round's new head comes back through this step).
//! - The gate log shown in the session's thread (`showGateLog`), the gate lock and
//!   `FactoryGate.run`'s checkout, leftover checkouts on disk and their removal
//!   (`removeLeftovers`; the runner's `gate_cleanup_failed` is reused, see below) and
//!   worktree removal (`worktree_removed`):
//!   replaced by the gate workspace, whose setup terminal the Factory view shows, and the
//!   forced archive of both workspaces.
//! - T3 thread status (`threadStatus`, `sessionBusy`, `THREAD_GONE` over a thread id): the
//!   fleet's session state and newest turn; a session the fleet does not show is `session-gone`.
//! - `gate refuses a run from another repo` and `gate on a run with no file`: a `RunId` names
//!   the repo and the ledger is the store.
//! - The issue text judged again before a pass is recorded (`textStop`, the cases about a
//!   changed issue text and a history that cannot be judged, and the approval on record): the
//!   text check is `watch`'s, which another task owns, as in `building`.
//! - `readyBy` (who marked a pull request ready): the tracker has no such read, so
//!   `pr_ready_outside` is written with no `by`.
//! - `apply marks the PR ready and posts the gate results once` and the other `apply:` cases:
//!   `apply_actions` is task 22's and is tested there.
//! - The forward-wait cases (`gate: waiting for the run a forwarded message starts`): a
//!   forward is another step's, the gate only waits while the session works.
//! - The runner's command-id cases are ported under their own names (`the second failure is
//!   sent with its own command id and the ordinal it read`, and the two "same command id"
//!   cases): `send_once`'s key and prompt count replace the id.
//! - `a PR out of draft is only accepted after the pass was recorded` and `a PR that is not a
//!   draft is refused when no attempt is open`: `building` puts a pull request that is out of
//!   draft back to draft when the session reports it, and the gate notes one marked ready
//!   while it ran (`pr_ready_outside`) instead of refusing it.
//! - `advance hands a recorded pass to the gate without looking at the PR`: `building`'s step.
//! - `a failed re-gate relaunches on that base, records it, and gates the fix against it`,
//!   `a ledger whose last gate line is a base-move gate_regate resumes the attempt and passes
//!   once` and the other re-gate cases of the `gate:` groups: `regate`.
//! - `the changed text path of the gate survives a failure of each call`, `the issue text is
//!   checked again before a pass is recorded and a change stops the run`, `a history that
//!   cannot be judged stops the gate with the same fixed reason as watch` and `a run with no
//!   approval on record is not marked ready`: the text check, as above.
//! - `a gate that cannot start a command is an error and writes no command line`, `a
//!   transient T3 error at gate time is an error and writes nothing` and `a thread that is
//!   gone at gate time sends the issue to stuck`'s T3 half: a gate that cannot be placed or
//!   started is `gate::could_not_run`, which backs off (`foundrys_own`).
//!
//! Runner cases whose Foundry counterpart has another name, because Foundry's behaviour
//! changes it:
//!
//! - `a PR marked ready by someone else while the gate ran still passes and names who` and `a
//!   PR marked ready outside is noted without who when GitHub names no one or cannot say`:
//!   `a_pr_marked_ready_outside_while_the_gate_ran_still_passes_and_is_noted_without_who`.
//! - `a head that moved while the checks ran is stuck and nothing is marked ready`:
//!   `a_head_that_moved_while_the_checks_ran_is_gated_again_and_nothing_is_marked_ready`
//!   (the spec gates a moved head again).
//! - `closed by the merge of a pull request whose pass is recorded is finished in two steps`:
//!   `closed_by_the_merge_of_a_pull_request_whose_pass_is_recorded_is_finished_without_marking_it_ready`
//!   (one pass finishes it as it stands, and ending the run is the feedback step's).
//!
//! Where the gate's output goes. Every gate's tail is written as the runner's
//! `gate_cleanup_failed { dir: <gate workspace name>, error: <tail> }` event, the only event
//! with a text field of its own that the ledger cuts and hides by itself, so that no cut or
//! hiding can reach the short `gate-ended <k> <code>` verdict and untrusted output is never
//! a `step_changed` that `read_progress` could read as a marker. The name is not what it says:
//! the export and a runner reader show it as a failed cleanup of a checkout named like the
//! gate, `Seen::cleanup_failed` fills with gate workspace names, and nothing in Foundry reads
//! it to decide a cleanup. The gate's number is found by comparing `dir` with
//! `gate::name(run, k)`. A dedicated gate-output event belongs in `ledger/events.rs` and is a
//! follow-up outside this issue's files.
//!
//! Copied from `building.rs`, whose own are private and which this issue may not edit, so a
//! change to one is to be made in both until they are made `pub(super)` there or move into
//! `runs/mod.rs` (a follow-up): `check_binding` with its `Unbound`, `reported_but` and the
//! words of `does_not_exist`; and the back-to-draft rule, which `back_to_draft` repeats
//! around `RunContext::mark_pr_draft`. Whether `gh` got no answer from GitHub is not copied:
//! `github::gh` gives such a failure the kind `OtherKind::NoAnswer`, which both read.
//!
//! `start.rs` counts a gate in flight as a `gate_checkout` with no `gate_finished`, which
//! disagrees with `gate_running` during a backoff: a gate that could not run has a checkout and
//! no `gate_finished` (none is written for it) and holds no slot. Task 30 should fold that
//! rule into `gating::gate_running`.
//!
//! A created gate the fleet does not show is given `UNSEEN_PASSES` passes, and not a time on the
//! sleep-counting clock, because what it waits for is the record to sync, which is a matter of
//! the passes (each pass reads a fresh view) and not of seconds; a pass that cannot read the
//! fleet is an error and is not counted.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use foundry_proto::factory::{RunEvent, RunId, SessionTurn};
use foundry_proto::ids::{MachineId, SessionId, WorkspaceId};
use foundry_proto::settings::SharedSettings;
use foundry_proto::state::{SessionState, TerminalState};

use super::progress::{
    CHECKS_TIMED_OUT, GATE_PASSED, GATE_REQUESTED, GATE_SENDING, PassRecord, Seen, UNREACHABLE_FOR,
    read_progress, step_text,
};
use super::{
    ActionPayload, Closing, Progress, RunContext, RunError, SendPayload, Stop, StuckStop, abandon,
    apply_actions, closed_issue, end_own_merge, end_run, go_stuck, progress_from, recorded_reason,
    session_id, workspace_id,
};
use crate::factory::config;
use crate::factory::gate::{self, CommandResult, GateResult};
use crate::factory::github::{
    CheckState, CheckStatus, Mergeable, OtherKind, PullRequest, TrackerError,
};
use crate::factory::ledger::cut_output;
use crate::factory::ledger::events::{Cause, CauseStep, CheckRead, Event, names};
use crate::factory::lifecycle::{Action, Event as LifecycleEvent, State, transition};
use crate::factory::messages::{
    self, CommandFailure, FailedCheck, GateFailure, Message, SESSION_GONE, STOPPED_BY_HAND,
    prepare_output_tail, prepare_session_text,
};
use crate::factory::placing::{self, Slots};
use crate::factory::text::{is_commit_id, sanitize};
use crate::factory::worker::{Target, WorkerError};
use crate::sessions::factory::BODY_PATH;

/// The step written once the worker has answered the create: `gate-created <k> <workspace id>`.
pub const GATE_CREATED: &str = "gate-created ";
/// The step that records a gate's verdict: `gate-ended <k> <exit code>`. The setup terminal's
/// tail is kept before it, in the `error` of a `gate_cleanup_failed` event whose `dir` is the
/// gate's workspace name: a field the ledger cuts on its own, so no cut or hiding can reach
/// the verdict.
pub const GATE_ENDED: &str = "gate-ended ";
/// The step written before a failed attempt is sent back: `gate-failing <attempt> <kind>`.
pub const GATE_FAILING: &str = "gate-failing ";
/// The step written when a gate's archive is first answered "unreachable" under a lease
/// generation: `gate-archive-waiting <k> <generation> <ms>`, `ms` read on the sleep-counting
/// clock.
pub const GATE_ARCHIVE_WAITING: &str = "gate-archive-waiting ";
/// The step of a forced archive that failed other than as unreachable: `gate-archive-failed <k>`.
pub const GATE_ARCHIVE_FAILED: &str = "gate-archive-failed ";
/// How many such failures make a gate workspace one that can never be archived.
pub const ARCHIVE_TRIES: u32 = 3;
/// The step that records a gate workspace as left, to be found by `resume::orphans` or a
/// person: `gate-left <k> <workspace id or -> <why>`, the why being fixed words.
pub const GATE_LEFT: &str = "gate-left ";
/// The step of a pass that found a created gate's workspace or setup terminal missing from
/// the fleet while its machine is online: `gate-unseen <k>`.
pub const GATE_UNSEEN: &str = "gate-unseen ";
/// How many such passes in all, the one that records it included, make a created gate one
/// that could not run.
pub const UNSEEN_PASSES: u32 = 6;
/// The step that starts the wait for the required checks, on the sleep-counting clock:
/// `checks-waiting <lease generation> <clock reading in ms>`.
pub const CHECKS_WAITING: &str = "checks-waiting ";
/// The step written before the gate workspace is archived: `gate-archiving <k>`.
pub const GATE_ARCHIVING: &str = "gate-archiving ";
/// The step written after it was: `gate-archived <k>`.
pub const GATE_ARCHIVED: &str = "gate-archived ";
/// The step of a gate that could not run:
/// `gate-could-not-run <attempt> <k> <failures> <wait> <reason>`, `wait` the passes to wait.
pub const GATE_COULD_NOT_RUN: &str = "gate-could-not-run ";
/// The step of a pass spent waiting after such a failure: `gate-waiting <failures>`.
pub const GATE_WAITING: &str = "gate-waiting ";
/// The step of a gate whose pull request head moved: `gate-head-moved <attempt> <head now>`.
pub const GATE_HEAD_MOVED: &str = "gate-head-moved ";
/// The step written once the pull request was marked ready at the pass:
/// `gate-ready-marked <attempt>`. Written after the call, which is safe to repeat, so that a
/// pass that waits after it does not mark the pull request ready again.
pub const GATE_READY_MARKED: &str = "gate-ready-marked ";
/// The step written once the build session is ended at the pass.
pub const BUILD_SESSION_ENDED: &str = "build-session-ended";
/// The step written when the end of the build session is first answered "unreachable" under
/// a lease generation: `build-session-waiting <generation> <ms>`, `ms` read on the
/// sleep-counting clock.
pub const BUILD_SESSION_WAITING: &str = "build-session-waiting ";
/// The step of a build session whose end was given up on because its machine is gone or did
/// not answer: `build-session-left <why>`, in fixed words.
pub const BUILD_SESSION_LEFT: &str = "build-session-left ";
/// The step written before the build workspace is archived at the pass.
pub const BUILD_ARCHIVING: &str = "build-archiving";
/// The step written once it was.
pub const BUILD_ARCHIVED: &str = "build-archived";
/// The step written for each refusal of the build workspace's archive that is not "unreachable".
pub const BUILD_ARCHIVE_FAILED: &str = "build-archive-failed";
/// The step written when the build workspace's archive is first answered "unreachable" under a
/// lease generation: `build-archive-waiting <generation> <ms>`, `ms` read on the
/// sleep-counting clock.
pub const BUILD_ARCHIVE_WAITING: &str = "build-archive-waiting ";
/// The step of a build workspace given up on because its machine is gone:
/// `build-left <why>`, in fixed words.
pub const BUILD_LEFT: &str = "build-left ";

/// The public reason of a run whose pull request head kept moving until no retry was left.
pub const HEAD_KEPT_MOVING: &str = "the pull request's head kept moving, so the gate could not settle on a commit within gate_retries";

/// The public reason of a failure that has no session to go back to: the build session was
/// ended at a pass that was then undone.
pub const SESSION_ENDED: &str = "the build session was ended when the pass was begun, so a failure after it has no session to go back to";

/// The sentence added to a failure sent back and to a stuck comment when the `.dw/config.yml`
/// at the pull request's head is not the default branch's.
pub const CONFIG_DIFFERS: &str = "This pull request changes .dw/config.yml, but the factory gates with the default branch's copy, so a change to provision or gate here has no effect until it is on the default branch.";

/// What one pass of [`advance`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GateStep {
    /// Every command passed, the pull request is ready, the label is `pr-open`, the build
    /// session is ended and its workspace archived. The run stays open for feedback.
    Passed { pr: u64 },
    /// A failed attempt was sent into the session; the gate waits for it to report again.
    SentBack { attempt: u32 },
    /// The run went stuck; `reason` is what the ledger records.
    Stuck { reason: String },
    /// Nothing to gate yet: no pull request reported, the run not building, or its session at
    /// work. Also the answer of a pass that found the head moved (the next pass gates the new
    /// head) and of a failed check read while the session works.
    NotReady,
    /// The pull request was merged or closed (or its own merge closed the issue): the run ended.
    Ended,
    /// A person closed the issue: the run ended, nothing written on GitHub.
    Abandoned,
    /// The gate is placed and its setup terminal has not exited.
    Running,
    /// The gate passed and the required checks are not all in: read again next pass.
    ChecksPending,
    /// No gate slot is free: nothing was written or asked.
    Queued,
    /// A gate could not run; `passes_left` more passes pass before it is tried again.
    BackingOff { passes_left: u32 },
}

/// The `merge` command line a conflict is recorded under.
const MERGE_COMMAND: &str = "merge";

/// One pass over `run`, whose session reported its pull request. See the module's words.
pub fn advance(
    ctx: &RunContext<'_>,
    run: &RunId,
    shared: &SharedSettings,
    slots: &mut Slots,
) -> Result<GateStep, RunError> {
    let events = ctx.ledger.read(run)?;
    let seen = read_progress(&events);
    if seen.finished {
        return Ok(GateStep::NotReady);
    }
    let (Some(thread), Some(last)) = (seen.thread_id.clone(), seen.last_pr.clone()) else {
        return Ok(GateStep::NotReady);
    };
    let gate = Gate {
        ctx,
        run,
        shared,
        issue: run.issue(),
        pr: last.pr,
        url: last.url,
        session: session_id(&thread)?,
    };
    // A closed issue is a person's move, or the run's own merged pull request, which ends the
    // run here unless a recorded pass has the gate finish it.
    let (closing, _) = closed_issue(ctx, run, &seen)?;
    match closing {
        Closing::PersonsMove => {
            gate.archive_all()?;
            abandon(ctx, run, &seen, CauseStep::Gate)?;
            return Ok(GateStep::Abandoned);
        }
        Closing::OwnMerge(pull) if seen.pass.is_none() => {
            gate.archive_all()?;
            end_own_merge(ctx, run, &seen, CauseStep::Gate, &pull)?;
            return Ok(GateStep::Ended);
        }
        Closing::OwnMerge(_) | Closing::Open => {}
    }
    gate.pass(&events, slots)
}

/// Whether `events` show a gate in flight, which holds one of the fleet's gate slots: placed
/// (`gate-requested`) and neither archived (`gate-archived`) nor given up on
/// (`gate-could-not-run`) since.
pub fn gate_running(events: &[RunEvent]) -> bool {
    Log::read(events)
        .reqs
        .last()
        .is_some_and(|req| !req.archived && req.left.is_none() && !req.cnr)
}

// ----- what the gate's own steps say -----

/// One gate placed: `gate-requested`, and what followed it.
#[derive(Clone, Debug, Default)]
struct Req {
    attempt: u32,
    k: u32,
    sha: String,
    base: String,
    machine: String,
    created: Option<String>,
    ended: Option<(i32, String)>,
    archiving: bool,
    archived: bool,
    cnr: bool,
    /// The gate was recorded as left (`gate-left`).
    left: Option<String>,
    /// Forced archives that failed, other than as unreachable.
    archive_failed: u32,
    /// The lease generation and sleep-counting clock reading (ms) the archive was first
    /// answered "unreachable" at (`gate-archive-waiting`).
    archive_waiting: Option<(u64, u64)>,
    /// Passes that found the created gate's workspace or terminal missing from the fleet.
    unseen: u32,
}

/// A `gate-could-not-run` step.
#[derive(Clone, Debug)]
struct Cnr {
    attempt: u32,
    failures: u32,
    reason: String,
    /// How many gates had been requested when it was written.
    reqs_at: usize,
}

/// The gate's steps of the round the run is in, read from its events.
#[derive(Default)]
struct Log {
    reqs: Vec<Req>,
    cnrs: Vec<Cnr>,
    /// Passes spent waiting since the newest `gate-could-not-run`.
    waited: u32,
    /// Attempts whose head moved.
    moved: BTreeSet<u32>,
    /// What each failed attempt is, as `gate-failing` says.
    failing: BTreeMap<u32, String>,
    /// Attempts whose `gate_finished` is written.
    finished: BTreeSet<u32>,
    session_ended: bool,
    /// Attempts whose pass marked the pull request ready (`gate-ready-marked`).
    ready_marked: BTreeSet<u32>,
    /// The end of the build session was given up on (`build-session-left`).
    session_left: bool,
    /// As `Req::archive_waiting`, for the end of the build session (`build-session-waiting`).
    session_waiting: Option<(u64, u64)>,
    build_archiving: bool,
    build_archived: bool,
    build_failed: u32,
    /// As `Req::archive_waiting`, for the build workspace (`build-archive-waiting`).
    build_waiting: Option<(u64, u64)>,
    /// The build workspace was given up on (`build-left`).
    build_left: bool,
    /// The highest attempt and gate number of the whole run, and the attempt when this round
    /// began: a round's budget counts from `base_attempt`, a gate's name is never reused.
    base_attempt: u32,
    max_attempt: u32,
    max_k: u32,
    /// The output each gate left, by the gate's workspace name.
    tails: BTreeMap<String, String>,
    /// The run these steps are of.
    run: Option<RunId>,
    /// The `gate_command` lines written since the newest `gate-requested`, as (attempt, command).
    lines: Vec<(u32, String)>,
    /// The generation and sleep-counting clock reading (ms) the checks wait began at.
    checks_wait: Option<(u64, u64)>,
}

fn number(text: Option<&str>) -> Option<u32> {
    text?.parse().ok()
}

/// `<generation> <ms>`: a lease generation and a sleep-counting clock reading, as a waiting
/// step writes them.
fn clock_reading(text: &str) -> Option<(u64, u64)> {
    let (generation, ms) = text.split_once(' ')?;
    Some((generation.parse().ok()?, ms.parse().ok()?))
}

impl Log {
    /// The build session is ended, or given up on.
    fn session_done(&self) -> bool {
        self.session_ended || self.session_left
    }

    fn read(events: &[RunEvent]) -> Log {
        let mut log = Log {
            run: events.first().map(|event| event.run.clone()),
            ..Log::default()
        };
        for event in events {
            match event.event.as_str() {
                names::STATE_CHANGED => {
                    // A round starts again when the run comes back to building.
                    if event.data["to"] == State::Building.name() {
                        let (attempt, k) = (log.max_attempt, log.max_k);
                        log = Log {
                            run: log.run.take(),
                            base_attempt: attempt,
                            max_attempt: attempt,
                            max_k: k,
                            ..Log::default()
                        };
                    }
                }
                names::GATE_COMMAND | names::GATE_SENT_BACK | "gate_checkout" => {
                    if let Some(attempt) = event.data["attempt"].as_u64() {
                        let attempt = u32::try_from(attempt).unwrap_or(u32::MAX);
                        log.note(attempt, 0);
                        if event.event == names::GATE_COMMAND
                            && let Some(command) = event.data["command"].as_str()
                        {
                            log.lines.push((attempt, command.to_owned()));
                        }
                    }
                }
                "gate_cleanup_failed" => {
                    // The gate's output, in a field the ledger cuts on its own, under the
                    // gate's name.
                    if let (Some(dir), Some(error)) =
                        (event.data["dir"].as_str(), event.data["error"].as_str())
                    {
                        log.tails.insert(dir.to_owned(), error.to_owned());
                    }
                }
                "gate_finished" => {
                    if let Some(attempt) = event.data["attempt"].as_u64() {
                        log.finished
                            .insert(u32::try_from(attempt).unwrap_or(u32::MAX));
                    }
                }
                names::STEP_CHANGED => {
                    if let Some(step) = step_text(event) {
                        log.step(step);
                    }
                }
                _ => {}
            }
        }
        let tails: Vec<Option<String>> = log
            .reqs
            .iter()
            .map(|req| log.tail_of(req.k).cloned())
            .collect();
        for (req, tail) in log.reqs.iter_mut().zip(tails) {
            if let Some((_, text)) = &mut req.ended {
                *text = tail.unwrap_or_default();
            }
        }
        log
    }

    /// The output the gate `k` left, found by its workspace's name.
    fn tail_of(&self, k: u32) -> Option<&String> {
        let run = self.run.as_ref()?;
        self.tails.get(&gate::name(run, k))
    }

    fn note(&mut self, attempt: u32, k: u32) {
        self.max_attempt = self.max_attempt.max(attempt);
        self.max_k = self.max_k.max(k);
    }

    fn step(&mut self, step: &str) {
        if let Some(rest) = step.strip_prefix(GATE_REQUESTED) {
            let mut part = rest.split(' ');
            if let (Some(attempt), Some(k), Some(sha), Some(base), Some(machine)) = (
                number(part.next()),
                number(part.next()),
                part.next(),
                part.next(),
                part.next(),
            ) {
                self.note(attempt, k);
                self.lines.clear();
                self.reqs.push(Req {
                    attempt,
                    k,
                    sha: sha.into(),
                    base: base.into(),
                    machine: machine.into(),
                    ..Req::default()
                });
            }
        } else if let Some(rest) = step.strip_prefix(GATE_CREATED) {
            if let Some((k, id)) = rest.split_once(' ')
                && let Some(req) = self.req(number(Some(k)))
            {
                req.created = Some(id.into());
            }
        } else if let Some(rest) = step.strip_prefix(GATE_ENDED) {
            if let Some((k, code)) = rest.split_once(' ')
                && let Ok(code) = code.parse()
                && let Some(req) = self.req(number(Some(k)))
            {
                req.ended = Some((code, String::new()));
            }
        } else if let Some(k) = step.strip_prefix(GATE_ARCHIVING) {
            if let Some(req) = self.req(number(Some(k))) {
                req.archiving = true;
            }
        } else if let Some(k) = step.strip_prefix(GATE_ARCHIVED) {
            if let Some(req) = self.req(number(Some(k))) {
                req.archived = true;
            }
        } else if let Some(rest) = step.strip_prefix(GATE_COULD_NOT_RUN) {
            let mut part = rest.splitn(5, ' ');
            if let (Some(attempt), Some(k), Some(failures), Some(_wait), Some(reason)) = (
                number(part.next()),
                number(part.next()),
                number(part.next()),
                part.next(),
                part.next(),
            ) {
                self.note(attempt, k);
                if let Some(req) = self.req(Some(k)) {
                    req.cnr = true;
                }
                self.waited = 0;
                self.cnrs.push(Cnr {
                    attempt,
                    failures,
                    reason: reason.into(),
                    reqs_at: self.reqs.len(),
                });
            }
        } else if let Some(rest) = step.strip_prefix(GATE_LEFT) {
            let mut part = rest.splitn(3, ' ');
            let k = number(part.next());
            let why = part.nth(1).unwrap_or_default().to_owned();
            if let Some(req) = self.req(k) {
                req.left = Some(why);
            }
        } else if let Some(rest) = step.strip_prefix(GATE_ARCHIVE_WAITING) {
            if let Some((k, since)) = rest.split_once(' ')
                && let Some(req) = self.req(number(Some(k)))
            {
                req.archive_waiting = clock_reading(since);
            }
        } else if let Some(rest) = step.strip_prefix(BUILD_ARCHIVE_WAITING) {
            self.build_waiting = clock_reading(rest);
        } else if step.starts_with(BUILD_LEFT) {
            self.build_left = true;
        } else if let Some(k) = step.strip_prefix(GATE_ARCHIVE_FAILED) {
            if let Some(req) = self.req(number(Some(k))) {
                req.archive_failed += 1;
            }
        } else if let Some(k) = step.strip_prefix(GATE_UNSEEN) {
            if let Some(req) = self.req(number(Some(k))) {
                req.unseen += 1;
            }
        } else if step.starts_with(GATE_WAITING) {
            self.waited += 1;
        } else if let Some(rest) = step.strip_prefix(CHECKS_WAITING) {
            if let Some(since) = clock_reading(rest) {
                self.checks_wait = Some(since);
            }
        } else if step
            .strip_prefix(GATE_PASSED)
            .is_some_and(|rest| rest.starts_with(' '))
        {
            self.checks_wait = None;
        } else if let Some(rest) = step.strip_prefix(GATE_HEAD_MOVED) {
            if let Some(attempt) = number(rest.split(' ').next()) {
                self.moved.insert(attempt);
                self.note(attempt, 0);
            }
        } else if let Some(rest) = step.strip_prefix(GATE_FAILING) {
            if let Some((attempt, kind)) = rest.split_once(' ')
                && let Some(attempt) = number(Some(attempt))
            {
                self.note(attempt, 0);
                self.failing.insert(attempt, kind.to_owned());
            }
        } else if let Some(attempt) = step.strip_prefix(GATE_READY_MARKED) {
            if let Some(attempt) = number(Some(attempt)) {
                self.ready_marked.insert(attempt);
            }
        } else if step == BUILD_SESSION_ENDED {
            self.session_ended = true;
        } else if step.starts_with(BUILD_SESSION_LEFT) {
            self.session_left = true;
        } else if let Some(rest) = step.strip_prefix(BUILD_SESSION_WAITING) {
            self.session_waiting = clock_reading(rest);
        } else if step == BUILD_ARCHIVING {
            self.build_archiving = true;
        } else if step == BUILD_ARCHIVE_FAILED {
            self.build_failed += 1;
        } else if step == BUILD_ARCHIVED {
            self.build_archived = true;
        }
    }

    fn req(&mut self, k: Option<u32>) -> Option<&mut Req> {
        let k = k?;
        self.reqs.iter_mut().find(|req| req.k == k)
    }

    /// The could-not-run no gate has been requested since: a gate waiting to be tried again.
    fn active_cnr(&self) -> Option<&Cnr> {
        self.cnrs
            .last()
            .filter(|cnr| cnr.reqs_at == self.reqs.len())
    }
}

/// What failed an attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// A command of the gate failed.
    Command,
    /// GitHub says the pull request conflicts with its base.
    Conflict,
    /// A required check failed.
    Checks,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Command => "command",
            Kind::Conflict => "conflict",
            Kind::Checks => "checks",
        }
    }

    fn of(name: &str) -> Option<Kind> {
        [Kind::Command, Kind::Conflict, Kind::Checks]
            .into_iter()
            .find(|kind| kind.name() == name)
    }
}

/// `n` attempts as the stuck comment counts them: "1 attempt", "2 attempts".
fn attempts(n: u32) -> String {
    format!("{n} {}", if n == 1 { "attempt" } else { "attempts" })
}

/// Why a pull request is not the session's own, in `building`'s words.
struct Unbound {
    public: String,
    detail: Option<String>,
}

fn reported_but(pr: u64, what: &str) -> String {
    format!("the session reported PR #{pr} but {what}")
}

/// Binds a pull request to a run as `building` does: in this repo, not a fork, merging into
/// `base`, and from one of the run's `branches`.
fn check_binding(
    pull: Option<&PullRequest>,
    pr: u64,
    base: &str,
    branches: &[&str],
) -> Option<Unbound> {
    let plain = |what: &str| Unbound {
        public: reported_but(pr, what),
        detail: None,
    };
    let Some(pull) = pull else {
        return Some(plain("it does not exist"));
    };
    if pull.cross_repository {
        return Some(plain("it comes from a fork"));
    }
    if pull.base_ref != base {
        let run_base = prepare_session_text(base);
        return Some(Unbound {
            public: reported_but(pr, &format!("its base is not the base branch {run_base}")),
            detail: Some(reported_but(
                pr,
                &format!(
                    "its base {} is not the base branch {run_base}",
                    prepare_session_text(&pull.base_ref)
                ),
            )),
        });
    }
    if branches.is_empty() {
        return Some(plain("the session has no branch"));
    }
    if !branches.contains(&pull.head_ref.as_str()) {
        return Some(Unbound {
            public: reported_but(pr, "its branch is not this session's branch"),
            detail: Some(reported_but(
                pr,
                &format!(
                    "its branch {} is not this session's branch",
                    prepare_session_text(&pull.head_ref)
                ),
            )),
        });
    }
    None
}

/// What a forced archive of a gate came to: see `Gate::archive_gate`.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Archive {
    /// Archived, recorded left, or nothing more to ask this pass.
    Done,
    /// Answered "unreachable" by a machine not given up on yet: asked again next pass.
    Waiting,
    /// Refused `ARCHIVE_TRIES` times before `pr-open`, with what the worker said last.
    Refused(String),
}

/// Where a gate workspace is archived from: see `Gate::archive_gate`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Place,
    Exit,
    PrOpen,
}

const LEFT_GONE: &str = "its machine is gone";
const LEFT_UNSHOWN: &str = "no workspace was shown for it";
const LEFT_UNREACHABLE: &str = "its archive was unreachable";
const LEFT_REFUSED: &str = "its archive was refused";
const LEFT_CREATE_REFUSED: &str = "its create was refused";

/// A forced archive of `id`, where P1's answer that it is not a live workspace counts as done.
fn archive_call(ctx: &RunContext<'_>, id: WorkspaceId) -> Result<(), RunError> {
    match ctx.archive(id) {
        Err(RunError::Worker(WorkerError::Failed(said))) if not_live(&said) => Ok(()),
        done => done,
    }
}

/// Whether the worker's answer to an archive is P1's `WorkspaceError::Unknown`: the workspace
/// "is not a live workspace of this machine".
fn not_live(said: &str) -> bool {
    said.contains("is not a live workspace of this machine")
}

/// The stop of a gate that could not run: the factory's words in the comment, what the worker
/// or the placement said in the ledger's detail only.
fn could_not_run_stop(reason: &str) -> Stop {
    Stop {
        reason: "the gate could not run".into(),
        detail: Some(format!("the gate could not run: {reason}")),
        ..Stop::default()
    }
}

/// Whether the session can take a message now: only an idle one. One that works cannot, and
/// neither can one whose process ended (the interruption rule is `resume`'s).
fn can_take_a_message(state: SessionState) -> bool {
    state == SessionState::Idle
}

/// Whether the session is at work, so that nothing is gated or read of it yet. A session that
/// finished or failed is not: its last turn stands and is gated. Nor is one that cannot be
/// reached and was [`at_rest`] when last seen: the gate needs the pull request it reported, not
/// the session, and a gate in flight on its machine must be read to be found gone. Whether it can
/// be sent a message is `can_take_a_message`'s, which is stricter.
fn busy(state: SessionState, seen: &Seen, turn: Option<&SessionTurn>) -> bool {
    match state {
        SessionState::Idle | SessionState::Finished | SessionState::Failed => false,
        SessionState::Unreachable => !at_rest(seen, turn),
        _ => true,
    }
}

/// Whether nothing shows that a session on a machine that cannot be reached is at work there:
/// the fleet shows it as last seen, its newest turn read, no prompt after that turn and no
/// question, and the ledger has no message sent to it that the fleet does not show yet.
pub(super) fn at_rest(seen: &Seen, turn: Option<&SessionTurn>) -> bool {
    turn.is_some_and(|turn| {
        turn.turn > 0
            && !turn.question
            && turn.prompts <= turn.turn
            && seen.turn_read.is_some_and(|read| read >= turn.turn)
            && !seen
                .sends
                .values()
                .any(|send| send.count >= turn.prompts && send.may_have_typed())
    })
}

// ----- one pass -----

struct Gate<'a, 'c> {
    ctx: &'a RunContext<'c>,
    run: &'a RunId,
    shared: &'a SharedSettings,
    issue: u64,
    pr: u64,
    url: String,
    session: SessionId,
}

impl Gate<'_, '_> {
    fn cause(&self, reason: &str) -> Cause {
        Cause::new(CauseStep::Gate, reason)
    }

    fn step(&self, text: &str, reason: &str, attempt: Option<u32>) -> Result<(), RunError> {
        let cause = match attempt {
            Some(attempt) => self.cause(reason).attempt(attempt),
            None => self.cause(reason),
        };
        self.ctx.record_step(self.run, text, &cause)
    }

    fn record(&self, event: Event, reason: &str, attempt: u32) -> Result<(), RunError> {
        self.ctx
            .record(self.run, event, &self.cause(reason).attempt(attempt))
    }

    /// The ledger as it stands.
    fn fresh(&self) -> Result<(Seen, Log), RunError> {
        let events = self.ctx.ledger.read(self.run)?;
        Ok((read_progress(&events), Log::read(&events)))
    }

    fn at(&self, seen: &Seen) -> Progress {
        progress_from(self.run, CauseStep::Gate, seen, State::Building)
    }

    fn retries(&self) -> u64 {
        self.ctx.config.limits.gate_retries
    }

    fn base<'s>(&'s self, seen: &'s Seen) -> &'s str {
        seen.base.as_deref().unwrap_or(&self.ctx.config.base)
    }

    fn commands(&self) -> Vec<String> {
        self.ctx
            .config
            .provision
            .iter()
            .chain(&self.ctx.config.gate)
            .cloned()
            .collect()
    }

    fn build_workspace(&self, seen: &Seen) -> Result<Option<WorkspaceId>, RunError> {
        seen.worktree
            .as_deref()
            .or(seen.workspace.as_deref())
            .map(workspace_id)
            .transpose()
    }

    // ----- ending the run's own workspaces -----

    /// The machine the build workspace is on: its session's owner as the view shows it, else
    /// the machine the run was claimed on.
    fn build_machine(&self, seen: &Seen, workspace: WorkspaceId) -> Option<MachineId> {
        self.ctx
            .view
            .session_of(workspace)
            .map(|record| record.owner)
            .or_else(|| {
                let claimed = seen.machine.as_deref()?;
                uuid::Uuid::parse_str(claimed).ok().map(MachineId)
            })
    }

    /// Whether the view shows the build machine unreachable for `progress::UNREACHABLE_FOR`.
    fn build_machine_gone(&self, seen: &Seen, workspace: WorkspaceId) -> bool {
        self.build_machine(seen, workspace)
            .is_some_and(|machine| self.ctx.view.unreachable_for(machine) >= UNREACHABLE_FOR)
    }

    /// The build session ended, once, by the build workspace's rule (`archive_build`): on a
    /// gone machine it is recorded left (`build-session-left <why>`) with no call, and an
    /// "unreachable" answer is waited out on the sleep-counting clock
    /// (`build-session-waiting <generation> <ms>`) for `progress::UNREACHABLE_FOR`, then
    /// recorded left. On an exit path (`exit`: the run ends or goes stuck) an "unreachable"
    /// answer is recorded left at once, as a gate's archive is. `false` while it is waited
    /// out.
    fn end_session(&self, exit: bool) -> Result<bool, RunError> {
        let (seen, log) = self.fresh()?;
        if log.session_done() {
            return Ok(true);
        }
        if let Some(workspace) = self.build_workspace(&seen)? {
            let left = |why: &str| {
                self.step(
                    &format!("{BUILD_SESSION_LEFT}{why}"),
                    "build-session-left",
                    None,
                )
                .map(|()| true)
            };
            if self.build_machine_gone(&seen, workspace) {
                return left(LEFT_GONE);
            }
            match self.ctx.end_sessions(workspace) {
                Ok(()) => {}
                Err(RunError::Worker(WorkerError::Unreachable(_))) => {
                    if exit
                        || self.waited(log.session_waiting, BUILD_SESSION_WAITING, None)?
                            >= UNREACHABLE_FOR
                    {
                        return left(LEFT_UNREACHABLE);
                    }
                    return Ok(false);
                }
                Err(error) => return Err(error),
            }
        }
        self.step(BUILD_SESSION_ENDED, "build-session-ended", None)?;
        Ok(true)
    }

    /// The build workspace archived by force, by the rule of `archive_gate` with the build
    /// machine's 15 minutes (`progress::UNREACHABLE_FOR`): `Some` is the step the pass ends
    /// with instead, `NotReady` while an "unreachable" answer is waited out.
    fn archive_build(&self) -> Result<Option<GateStep>, RunError> {
        let (seen, log) = self.fresh()?;
        if log.build_archived || log.build_left {
            return Ok(None);
        }
        if let Some(workspace) = self.build_workspace(&seen)? {
            let left = |why: &str| {
                self.step(&format!("{BUILD_LEFT}{why}"), "build-left", None)
                    .map(|()| None)
            };
            if self.build_machine_gone(&seen, workspace) {
                return left(LEFT_GONE);
            }
            if !log.build_archiving {
                self.step(BUILD_ARCHIVING, "build-archiving", None)?;
            }
            match archive_call(self.ctx, workspace) {
                Ok(()) => {}
                Err(error @ RunError::LeaseLost(_)) => return Err(error),
                Err(RunError::Worker(WorkerError::Unreachable(_))) => {
                    let waited = self.waited(log.build_waiting, BUILD_ARCHIVE_WAITING, None)?;
                    if waited >= UNREACHABLE_FOR {
                        return left(LEFT_UNREACHABLE);
                    }
                    return Ok(Some(GateStep::NotReady));
                }
                Err(error) => {
                    self.step(BUILD_ARCHIVE_FAILED, "build-archive-failed", None)?;
                    if log.build_failed + 1 >= ARCHIVE_TRIES {
                        return Ok(Some(self.archive_stuck(
                            "build-archive-failed",
                            "the build workspace could not be archived",
                            &error.to_string(),
                        )?));
                    }
                    return Err(error);
                }
            }
        }
        self.step(BUILD_ARCHIVED, "build-archived", None)?;
        Ok(None)
    }

    /// How long a wait has lasted on the sleep-counting clock, from the reading `since`
    /// recorded under this lease generation. With none, or one of another generation or above
    /// the clock now (a takeover, or a reboot that reset the clock), a new start is recorded
    /// as `<prefix><generation> <ms>` and the wait is zero: a takeover can lengthen a wait,
    /// never end it early.
    fn waited(
        &self,
        since: Option<(u64, u64)>,
        prefix: &str,
        attempt: Option<u32>,
    ) -> Result<Duration, RunError> {
        let generation = self.ctx.held()?;
        let now = u64::try_from(self.ctx.clock.now().as_millis()).unwrap_or(u64::MAX);
        if let Some(waited) = super::time_waited(since, generation, now) {
            return Ok(waited);
        }
        let reason = prefix.split(' ').next().unwrap_or(prefix);
        self.step(&format!("{prefix}{generation} {now}"), reason, attempt)?;
        Ok(Duration::ZERO)
    }

    /// The run is stuck: a workspace of it can never be archived, which the issue's acceptance
    /// criterion (none left at `pr-open`) does not allow to pass. `what` names the workspace
    /// in the factory's words.
    fn archive_stuck(&self, code: &str, what: &str, error: &str) -> Result<GateStep, RunError> {
        let stop = Stop {
            reason: what.into(),
            detail: Some(format!("{what}: {}", error.replace('\n', " "))),
            ..Stop::default()
        };
        self.stuck(code, stop)
    }

    /// The stuck path, with the build session ended first and once.
    fn stuck(&self, code: &str, stop: Stop) -> Result<GateStep, RunError> {
        self.end_session(true)?;
        self.archive_all()?;
        let (seen, _) = self.fresh()?;
        let mut at = self.at(&seen);
        // Ended above: `go_stuck` has nothing to stop.
        at.worktree = None;
        let reason = recorded_reason(&stop);
        go_stuck(self.ctx, self.issue, &mut at, code, stop, None)?;
        Ok(GateStep::Stuck { reason })
    }

    /// A stuck decision an earlier pass began, finished with the words it recorded.
    fn resume_stuck(&self, seen: &Seen) -> Result<GateStep, RunError> {
        let Some(raw) = &seen.stuck_stop else {
            let Some(reason) = &seen.stuck else {
                return Err(RunError::Invalid(format!(
                    "run {} has no stuck path",
                    self.run
                )));
            };
            self.stuck("stuck-resumed", Stop::new(reason.clone()))?;
            return Ok(GateStep::Stuck {
                reason: reason.clone(),
            });
        };
        let decision: StuckStop = serde_json::from_str(raw).map_err(|error| {
            RunError::Invalid(format!("the stuck decision of {}: {error}", self.run))
        })?;
        let mut stop = decision.stop;
        stop.detail = seen.stuck_detail.clone();
        if decision.code == "checks-retries-used-up"
            && let Some(failed) = &seen.checks_failed
        {
            let list = self.failed_checks(&failed.sha, &failed.failed);
            stop.session_text = Some(prepare_output_tail(&messages::checks_failure_text(
                &failed.sha,
                &list,
            )));
        }
        self.stuck(&decision.code, stop)
    }

    // ----- a gate's workspace -----

    /// One rule for a gate workspace. Only `archive` is ever called, never a create (which
    /// would start the pull request's commands), and `gate-archived` is written only after a
    /// call answered Ok or P1's "is not a live workspace of this machine", never from what the
    /// view shows or does not. A recorded id is always archived by call, whatever the view
    /// shows. With none recorded (the create failed or got no answer, and may have made one)
    /// every live workspace the view shows under the gate's name is. If none is shown, the
    /// gate waits `UNSEEN_PASSES` passes off an exit path, and after that, or on one, it is
    /// recorded `gate-left <k> <id or -> <why>` (fixed words) for `resume::orphans` or a person.
    ///
    /// A failed archive is, by `mode`: on an exit path (`Mode::Exit`: the run ends, is
    /// abandoned or goes stuck) recorded left at once and the run goes on. Otherwise a refusal
    /// is an error asked again next pass and counted: after `ARCHIVE_TRIES` of them the gate
    /// is recorded left while placing gates (`Mode::Place`), and before `pr-open`
    /// (`Mode::PrOpen`) the answer is [`Archive::Refused`]: the run is stuck, as the issue's
    /// acceptance criterion (no workspace of the run left at `pr-open`) requires, where the
    /// runner notes a leftover checkout it cannot remove and goes on. An "unreachable" answer
    /// is not an error and is not counted: it is [`Archive::Waiting`], asked again next pass,
    /// until it has been answered for `gate::MACHINE_GONE_AFTER` on the sleep-counting clock
    /// (`gate-archive-waiting <k> <generation> <ms>`, written at the first such answer), and
    /// then the gate is recorded left as a gone machine's is. A gate on a machine the view
    /// shows unreachable for `gate::MACHINE_GONE_AFTER` is recorded left without a call. A
    /// gate recorded left holds nothing; the spec has its machine archive it when heard from
    /// again.
    fn archive_gate(&self, req: &Req, mode: Mode) -> Result<Archive, RunError> {
        if req.archived {
            return Ok(Archive::Done);
        }
        // Only the `pr-open` sweep goes back to a gate that was left, and it decides from what
        // the view shows now, not from why the gate was left.
        if req.left.is_some() && mode != Mode::PrOpen {
            return Ok(Archive::Done);
        }
        let ctx = self.ctx;
        let live = ctx
            .view
            .workspaces_named(ctx.project.id, &gate::name(self.run, req.k));
        let recorded = req.created.as_deref().map(workspace_id).transpose()?;
        // What the view shows now, and the recorded id, whatever the gate was left for.
        let mut ids: Vec<WorkspaceId> = recorded.into_iter().collect();
        for record in &live {
            if !ids.contains(&record.body.id) {
                ids.push(record.body.id);
            }
        }
        let machine = live
            .first()
            .map(|record| record.owner)
            .or_else(|| uuid::Uuid::parse_str(&req.machine).ok().map(MachineId));
        let left = |why: &str| {
            if req.left.is_some() {
                return Ok(Archive::Done);
            }
            let named = ids.first().map_or("-".to_owned(), |id| id.0.to_string());
            self.step(
                &format!("{GATE_LEFT}{} {named} {why}", req.k),
                "gate-left",
                None,
            )
            .map(|()| Archive::Done)
        };
        if machine
            .is_some_and(|machine| ctx.view.unreachable_for(machine) >= gate::MACHINE_GONE_AFTER)
        {
            return left(LEFT_GONE);
        }
        if ids.is_empty() {
            // Nothing is shown: it may be made yet.
            if mode != Mode::Place || req.unseen + 1 >= UNSEEN_PASSES {
                return left(LEFT_UNSHOWN);
            }
            self.step(
                &format!("{GATE_UNSEEN}{}", req.k),
                "gate-unseen",
                Some(req.attempt),
            )?;
            return Ok(Archive::Done);
        }
        if !req.archiving {
            self.step(
                &format!("{GATE_ARCHIVING}{}", req.k),
                "gate-archiving",
                None,
            )?;
        }
        for id in ids.clone() {
            match archive_call(ctx, id) {
                Ok(()) => {}
                Err(error @ RunError::LeaseLost(_)) => return Err(error),
                Err(error) => {
                    let unreachable =
                        matches!(error, RunError::Worker(WorkerError::Unreachable(_)));
                    if mode == Mode::Exit {
                        return left(if unreachable {
                            LEFT_UNREACHABLE
                        } else {
                            LEFT_REFUSED
                        });
                    }
                    if unreachable {
                        let prefix = format!("{GATE_ARCHIVE_WAITING}{} ", req.k);
                        if self.waited(req.archive_waiting, &prefix, None)?
                            >= gate::MACHINE_GONE_AFTER
                        {
                            return left(LEFT_UNREACHABLE);
                        }
                        return Ok(Archive::Waiting);
                    }
                    self.step(
                        &format!("{GATE_ARCHIVE_FAILED}{}", req.k),
                        "gate-archive-failed",
                        None,
                    )?;
                    if req.archive_failed + 1 >= ARCHIVE_TRIES {
                        return match mode {
                            Mode::PrOpen => Ok(Archive::Refused(error.to_string())),
                            _ => left(LEFT_REFUSED),
                        };
                    }
                    return Err(error);
                }
            }
        }
        self.step(&format!("{GATE_ARCHIVED}{}", req.k), "gate-archived", None)?;
        Ok(Archive::Done)
    }

    /// Every gate workspace of the round that is not archived or left, on an exit path.
    fn archive_all(&self) -> Result<(), RunError> {
        let (_, log) = self.fresh()?;
        for req in log
            .reqs
            .iter()
            .filter(|req| !req.archived && req.left.is_none())
        {
            self.archive_gate(req, Mode::Exit)?;
        }
        Ok(())
    }

    /// The gates of the round archived before the run is `pr-open`: `Some` is the step the
    /// pass ends with instead, `NotReady` while an "unreachable" answer is waited out and
    /// stuck for a workspace that can never be archived.
    fn sweep_for_pr_open(&self) -> Result<Option<GateStep>, RunError> {
        let (_, log) = self.fresh()?;
        for req in log.reqs.iter().filter(|req| !req.archived) {
            match self.archive_gate(req, Mode::PrOpen)? {
                Archive::Done => {}
                Archive::Waiting => return Ok(Some(GateStep::NotReady)),
                Archive::Refused(error) => {
                    return Ok(Some(self.archive_stuck(
                        "gate-archive-failed",
                        "a gate workspace could not be archived",
                        &error,
                    )?));
                }
            }
        }
        Ok(None)
    }

    // ----- the pass -----

    /// The pass over `events`, the ledger `advance` read: nothing is written between the two.
    fn pass(&self, events: &[RunEvent], slots: &mut Slots) -> Result<GateStep, RunError> {
        let seen = read_progress(events);
        let log = Log::read(events);
        if seen.stuck.is_some() || seen.stuck_stop.is_some() {
            return self.resume_stuck(&seen);
        }
        if seen.stop_requested && seen.state == Some(State::Building) {
            return self.stuck("stop-requested", Stop::new(STOPPED_BY_HAND));
        }
        match seen.state {
            Some(State::PrOpen) => {
                // A pass that finished everything decides from the ledger it holds and reads
                // nothing more: a `pr-open` run is passed over on every pass.
                let finished = log.session_done()
                    && (log.build_archived || log.build_left)
                    && log
                        .reqs
                        .iter()
                        .all(|req| req.archived || req.left.is_some());
                if finished {
                    return Ok(GateStep::Passed { pr: self.pr });
                }
                if !self.end_session(false)? {
                    return Ok(GateStep::NotReady);
                }
                if let Some(step) = self.sweep_for_pr_open()? {
                    return Ok(step);
                }
                if let Some(step) = self.archive_build()? {
                    return Ok(step);
                }
                return Ok(GateStep::Passed { pr: self.pr });
            }
            Some(State::Building) => {}
            _ => return Ok(GateStep::NotReady),
        }
        let pass = seen
            .pass
            .clone()
            .filter(|pass| !log.moved.contains(&pass.attempt));
        let state = self.ctx.view.session_state(self.session);
        let shown = self.ctx.view.session_turn(self.session);
        let turn = shown.map(|t| t.turn);
        // A send begun and not recorded is finished first, whatever else moved.
        if let Some(owed) = seen
            .sending
            .filter(|sending| !seen.sent_back_attempts.contains(&sending.attempt))
        {
            let Some(state) = state else {
                return self.stuck("session-gone", Stop::new(SESSION_GONE));
            };
            if !can_take_a_message(state) || turn.is_none() {
                return Ok(GateStep::NotReady);
            }
            let kind = log
                .failing
                .get(&owed.attempt)
                .and_then(|kind| Kind::of(kind))
                .ok_or_else(|| {
                    RunError::Invalid(format!("attempt {} has no failure on record", owed.attempt))
                })?;
            return self.send_back(owed.attempt, kind, turn);
        }
        match pass {
            Some(pass) => self.after_pass(&seen, &log, &pass, state, turn),
            None => {
                let Some(state) = state else {
                    return self.stuck("session-gone", Stop::new(SESSION_GONE));
                };
                if busy(state, &seen, shown) || turn.is_none() || !seen.pr_reported {
                    return Ok(GateStep::NotReady);
                }
                self.attempt(&seen, &log, slots, turn)
            }
        }
    }

    // ----- placing and reading a gate -----

    /// No pass is recorded: the gate in flight is read, a verdict settled, or a gate placed.
    fn attempt(
        &self,
        seen: &Seen,
        log: &Log,
        slots: &mut Slots,
        turn: Option<u64>,
    ) -> Result<GateStep, RunError> {
        let closed = |attempt: u32| {
            seen.sent_back_attempts.contains(&attempt)
                || seen.passed_attempts.contains(&attempt)
                || log.moved.contains(&attempt)
        };
        match log.reqs.last() {
            Some(req) if !req.cnr && req.ended.is_none() => return self.read_gate(req.clone()),
            Some(req) if req.ended.is_some() && !closed(req.attempt) => {
                return self.settle(req.clone(), turn);
            }
            _ => {}
        }
        let latest = log
            .max_attempt
            .max(seen.command_attempts.last().copied().unwrap_or(0))
            .max(seen.sent_back_attempts.last().copied().unwrap_or(0));
        let attempt = if latest == 0 {
            1
        } else if closed(latest) {
            latest + 1
        } else {
            latest
        };
        // A gate that could not run is archived before another is placed.
        for req in log
            .reqs
            .iter()
            .filter(|req| req.cnr && !req.archived && req.left.is_none())
        {
            self.archive_gate(req, Mode::Place)?;
        }
        // A gate that could not run waits its passes out.
        if let Some(cnr) = log.active_cnr() {
            match gate::backoff(cnr.failures) {
                None => return self.stuck("gate-could-not-run", could_not_run_stop(&cnr.reason)),
                Some(wait) if log.waited < wait => {
                    self.step(
                        &format!("{GATE_WAITING}{}", cnr.failures),
                        "gate-waiting",
                        Some(attempt),
                    )?;
                    return Ok(GateStep::BackingOff {
                        passes_left: wait - log.waited - 1,
                    });
                }
                Some(_) => {}
            }
        }
        // A gate that could not run and has no workspace shown to archive may have made one:
        // nothing is placed until it is archived or recorded left.
        let (_, now) = self.fresh()?;
        if now
            .reqs
            .iter()
            .any(|req| req.cnr && !req.archived && req.left.is_none())
        {
            let left = now
                .reqs
                .iter()
                .filter(|req| req.cnr && !req.archived && req.left.is_none())
                .map(|req| UNSEEN_PASSES.saturating_sub(req.unseen + 1))
                .max()
                .unwrap_or(0);
            return Ok(GateStep::BackingOff { passes_left: left });
        }
        let pull = self.ctx.tracker.pull_request(self.ctx.repo(), self.pr)?;
        if let Some(step) = self.unbound(seen, pull.as_ref())? {
            return Ok(step);
        }
        let pull = pull.ok_or_else(|| RunError::Invalid("a bound pull request is gone".into()))?;
        if !pull.open {
            self.archive_all()?;
            let at = self.at(seen);
            end_run(self.ctx, &at, &self.url, Some(&pull), CauseStep::Gate)?;
            return Ok(GateStep::Ended);
        }
        if u64::from(attempt - log.base_attempt) > self.retries() + 1 {
            return self.stuck("head-kept-moving", Stop::new(HEAD_KEPT_MOVING));
        }
        let head = pull.head_sha.to_lowercase();
        if !is_commit_id(&head) {
            return Err(RunError::Invalid(
                "the pull request's head is not a commit id".into(),
            ));
        }
        if self.ctx.config.gate.is_empty() {
            return Err(RunError::Invalid(
                "the project config names no gate command".into(),
            ));
        }
        // GitHub already says the head conflicts: the attempt failed, with no gate or slot.
        if pull.mergeable == Mergeable::Conflicting {
            if !seen.command_attempts.contains(&attempt) {
                self.record(
                    Event::GateCommand {
                        attempt,
                        command: MERGE_COMMAND.into(),
                        exit_code: 1,
                        duration_ms: 0,
                    },
                    "gate-merge-conflict",
                    attempt,
                )?;
            }
            return self.fail_attempt(attempt, Kind::Conflict, Some(pull), turn);
        }
        if slots.gates_free == 0 {
            return Ok(GateStep::Queued);
        }
        let base_sha = self
            .ctx
            .tracker
            .branch_tip(self.ctx.repo(), self.base(seen))?
            .to_lowercase();
        let k = log.max_k + 1;
        let target = match placing::place_gate(self.ctx.view, self.shared, &self.ctx.project.login)
        {
            Ok(target) => target,
            Err(reasons) => {
                let reason = format!("no machine can take the gate: {}", reasons.join("; "));
                return self.could_not_run(log, attempt, k, &reason);
            }
        };
        slots.gates_free -= 1;
        self.step(
            &format!("{GATE_REQUESTED}{attempt} {k} {head} {base_sha} {target}"),
            "gate-requested",
            Some(attempt),
        )?;
        let req = Req {
            attempt,
            k,
            sha: head,
            base: base_sha,
            machine: target.to_string(),
            ..Req::default()
        };
        self.read_gate(req)
    }

    /// A gate that could not run: recorded, and waited out, or the run is stuck.
    fn could_not_run(
        &self,
        log: &Log,
        attempt: u32,
        k: u32,
        reason: &str,
    ) -> Result<GateStep, RunError> {
        self.could_not_run_left(log, attempt, k, reason, None)
    }

    /// [`Self::could_not_run`], with the gate recorded left for `left` right after the
    /// `gate-could-not-run` step: a pass cut between the two leaves a gate that could not run,
    /// which is archived or left before another is placed, and never one left that a later
    /// pass would read as in flight.
    fn could_not_run_left(
        &self,
        log: &Log,
        attempt: u32,
        k: u32,
        reason: &str,
        left: Option<&str>,
    ) -> Result<GateStep, RunError> {
        let reason = reason.replace('\n', " ");
        let failures = u32::try_from(log.cnrs.iter().filter(|c| c.attempt == attempt).count())
            .unwrap_or(u32::MAX)
            + 1;
        let wait = gate::backoff(failures);
        let shown = wait.map_or("-".to_owned(), |wait| wait.to_string());
        self.step(
            &format!("{GATE_COULD_NOT_RUN}{attempt} {k} {failures} {shown} {reason}"),
            "gate-could-not-run",
            Some(attempt),
        )?;
        if let Some(why) = left {
            self.step(&format!("{GATE_LEFT}{k} - {why}"), "gate-left", None)?;
        }
        match wait {
            Some(passes_left) => Ok(GateStep::BackingOff { passes_left }),
            None => self.stuck("gate-could-not-run", could_not_run_stop(&reason)),
        }
    }

    /// The gate `req` placed: asked for if it was not, then its terminal read.
    fn read_gate(&self, req: Req) -> Result<GateStep, RunError> {
        let ctx = self.ctx;
        let bad_machine = || RunError::Invalid(format!("the gate's machine {:?}", req.machine));
        let target: Target = req.machine.parse().map_err(|_| bad_machine())?;
        let name = gate::name(self.run, req.k);
        let found = ctx
            .view
            .workspace_named(ctx.project.id, &name)
            .map(|record| (record.body.id, record.body.setup, record.owner));
        let (gate_workspace, setup, owner) = match found {
            Some((id, setup, owner)) => {
                if req.created.is_none() {
                    self.created(&req, id)?;
                }
                (id, setup, owner)
            }
            None if req.created.is_none() => {
                let request = gate::request(ctx.config, ctx.project.id, &req.sha, self.run, req.k);
                // The session reported its pull request: its workspace is put at rest, so the
                // gate has room. A create asked again rests it again, which is safe.
                let (seen, _) = self.fresh()?;
                if let Some(workspace) = self.build_workspace(&seen)? {
                    ctx.rest(workspace)?;
                }
                return match ctx.create(target, request) {
                    Ok(id) => {
                        self.created(&req, id)?;
                        Ok(GateStep::Running)
                    }
                    Err(RunError::Worker(error)) => {
                        let reason = gate::could_not_run(Some(&error), Duration::ZERO, None)
                            .unwrap_or_else(|| error.to_string());
                        let (_, log) = self.fresh()?;
                        // A refusal made nothing (P1 answers a workspace it made with its id):
                        // nothing waits to be found, so the gate is left at once.
                        let refused = matches!(error, WorkerError::Failed(_));
                        self.could_not_run_left(
                            &log,
                            req.attempt,
                            req.k,
                            &reason,
                            refused.then_some(LEFT_CREATE_REFUSED),
                        )
                    }
                    Err(error) => Err(error),
                };
            }
            None => {
                // The workspace's record has not reached the view, or is gone from it.
                let machine = target.machine().ok_or_else(bad_machine)?;
                let gone = gate::could_not_run(None, ctx.view.unreachable_for(machine), None);
                return self.unseen(&req, machine, gone);
            }
        };
        let terminal = setup.and_then(|terminal| {
            ctx.view
                .terminal(terminal)
                .map(|(state, code)| (terminal, state, code))
        });
        let gone = gate::could_not_run(
            None,
            ctx.view.unreachable_for(owner),
            terminal.map(|(_, state, code)| (state, code)),
        );
        if gone.is_some() {
            return self.running_or_gone(&req, gone);
        }
        if terminal.is_none() {
            return self.unseen(&req, owner, None);
        }
        let Some((terminal, TerminalState::Exited, Some(code))) = terminal else {
            ctx.advance(gate_workspace)?;
            return Ok(GateStep::Running);
        };
        // The output first, then the verdict: the verdict is what settles the gate, and the
        // output is read from the worker once.
        let (_, before) = self.fresh()?;
        if before.tail_of(req.k).is_none() {
            let tail = ctx.tail(terminal)?;
            self.record(
                Event::GateCleanupFailed {
                    dir: name.clone(),
                    error: cut_output(&tail).to_owned(),
                },
                "gate-output",
                req.attempt,
            )?;
        }
        self.step(
            &format!("{GATE_ENDED}{} {code}", req.k),
            "gate-ended",
            Some(req.attempt),
        )?;
        let (_, log) = self.fresh()?;
        let Some(ended) = log.reqs.iter().find(|r| r.k == req.k).cloned() else {
            return Err(RunError::Invalid(
                "the gate's verdict is not on record".into(),
            ));
        };
        let turn = ctx.view.session_turn(self.session).map(|t| t.turn);
        self.settle(ended, turn)
    }

    /// A created gate whose workspace or setup terminal the fleet does not show. A machine
    /// that is gone is `gone`'s; one that is online is given `UNSEEN_PASSES` passes for the
    /// record to arrive, and then the gate could not run.
    fn unseen(
        &self,
        req: &Req,
        machine: MachineId,
        gone: Option<String>,
    ) -> Result<GateStep, RunError> {
        if gone.is_some() || req.created.is_none() || !self.ctx.view.online(machine) {
            return self.running_or_gone(req, gone);
        }
        self.step(
            &format!("{GATE_UNSEEN}{}", req.k),
            "gate-unseen",
            Some(req.attempt),
        )?;
        if req.unseen + 1 < UNSEEN_PASSES {
            return Ok(GateStep::Running);
        }
        let (_, log) = self.fresh()?;
        self.could_not_run(
            &log,
            req.attempt,
            req.k,
            "the gate's workspace is not in the fleet",
        )
    }

    fn running_or_gone(&self, req: &Req, gone: Option<String>) -> Result<GateStep, RunError> {
        match gone {
            None => Ok(GateStep::Running),
            Some(reason) => {
                let (_, log) = self.fresh()?;
                self.could_not_run(&log, req.attempt, req.k, &reason)
            }
        }
    }

    fn created(&self, req: &Req, id: WorkspaceId) -> Result<(), RunError> {
        self.step(
            &format!("{GATE_CREATED}{} {}", req.k, id.0),
            "gate-created",
            Some(req.attempt),
        )?;
        self.record(
            Event::GateCheckout {
                attempt: req.attempt,
                dir: gate::name(self.run, req.k),
                sha: req.sha.clone(),
                base_sha: req.base.clone(),
            },
            "gate-checkout",
            req.attempt,
        )
    }

    // ----- a verdict on record -----

    /// The gate `req` has a verdict: its workspace archived, its commands recorded, and the
    /// verdict acted on.
    fn settle(&self, req: Req, turn: Option<u64>) -> Result<GateStep, RunError> {
        let Some((code, tail)) = req.ended.clone() else {
            return Err(RunError::Invalid("the gate has no verdict".into()));
        };
        // The verdict waits for the gate's workspace to be archived or left.
        if self.archive_gate(&req, Mode::Place)? == Archive::Waiting {
            return Ok(GateStep::NotReady);
        }
        let (result, commands) = self.read_tail(code, &tail)?;
        let failing = match &result {
            GateResult::Failed { command, .. } => {
                command.clone().unwrap_or_else(|| commands.join(" && "))
            }
            _ => String::new(),
        };
        let (seen, log) = self.fresh()?;
        let attempt = req.attempt;
        // The lines of this gate that are not yet on record: a pass cut part way is completed,
        // and the line of an earlier conflict is not this gate's.
        let written = log
            .lines
            .iter()
            .filter(|(line_attempt, _)| *line_attempt == attempt)
            .count();
        if code == 0 {
            for command in commands.iter().skip(written) {
                self.record(
                    Event::GateCommand {
                        attempt,
                        command: command.clone(),
                        exit_code: 0,
                        duration_ms: 0,
                    },
                    "gate-command",
                    attempt,
                )?;
            }
        } else if written == 0 {
            self.record(
                Event::GateCommand {
                    attempt,
                    command: failing.clone(),
                    exit_code: code,
                    duration_ms: 0,
                },
                "gate-command",
                attempt,
            )?;
        }
        if !log.finished.contains(&attempt) {
            self.record(
                Event::GateFinished {
                    attempt,
                    total_ms: 0,
                },
                "gate-finished",
                attempt,
            )?;
        }
        // What the pull request is now decides what the verdict means.
        let pull = self.ctx.tracker.pull_request(self.ctx.repo(), self.pr)?;
        if let Some(step) = self.unbound(&seen, pull.as_ref())? {
            return Ok(step);
        }
        let pull = pull.ok_or_else(|| RunError::Invalid("a bound pull request is gone".into()))?;
        let head = pull.head_sha.to_lowercase();
        if log.moved.contains(&attempt) {
            return self.after_moved(attempt, &log);
        }
        if !pull.open {
            if code == 0 && pull.merged && head == req.sha {
                self.record_pass(&req, &commands)?;
                let (seen, _) = self.fresh()?;
                let pass = seen
                    .pass
                    .ok_or_else(|| RunError::Invalid("the pass is not on record".into()))?;
                // The run is ended here only when the pass is finished in this pass. One that
                // waits on an archive or a session's end is finished by the next pass from the
                // pass on record, which leaves the run in `pr-open` for the feedback step to end
                // as merged (the runner's two steps).
                let finished = self.finish_pass(&pass, false)?;
                if !matches!(finished, GateStep::Passed { .. }) {
                    return Ok(finished);
                }
                let (seen, _) = self.fresh()?;
                let mut at = self.at(&seen);
                at.worktree = None;
                end_run(self.ctx, &at, &self.url, Some(&pull), CauseStep::Gate)?;
                return Ok(GateStep::Ended);
            }
            if pull.merged && head != req.sha {
                self.moved(attempt, &head, &log)?;
                return Ok(GateStep::NotReady);
            }
            let mut at = self.at(&seen);
            at.worktree = None;
            self.end_session(true)?;
            self.archive_all()?;
            end_run(self.ctx, &at, &self.url, Some(&pull), CauseStep::Gate)?;
            return Ok(GateStep::Ended);
        }
        if head != req.sha {
            // Nothing stays ready on a head that was not gated.
            if !pull.is_draft
                && let Some(step) = self.back_to_draft(&pull)?
            {
                return Ok(step);
            }
            self.moved(attempt, &head, &log)?;
            return self.after_moved(attempt, &log);
        }
        if code == 0 {
            if !pull.is_draft && !seen.ready_outside {
                self.record(
                    Event::PrReadyOutside {
                        pr: self.pr,
                        by: None,
                    },
                    "pr-ready-outside",
                    attempt,
                )?;
            }
            self.record_pass(&req, &commands)?;
            let (seen, log) = self.fresh()?;
            let pass = seen
                .pass
                .clone()
                .ok_or_else(|| RunError::Invalid("the pass is not on record".into()))?;
            let state = self.ctx.view.session_state(self.session);
            return self.after_pass(&seen, &log, &pass, state, turn);
        }
        self.fail_attempt(attempt, Kind::Command, Some(pull), turn)
    }

    fn record_pass(&self, req: &Req, commands: &[String]) -> Result<(), RunError> {
        let (seen, _) = self.fresh()?;
        if seen.passed_attempts.contains(&req.attempt) {
            return Ok(());
        }
        self.step(
            &format!("{GATE_PASSED} {} {}", req.sha, commands.len()),
            GATE_PASSED,
            Some(req.attempt),
        )
    }

    fn moved(&self, attempt: u32, head: &str, log: &Log) -> Result<(), RunError> {
        if log.moved.contains(&attempt) {
            return Ok(());
        }
        self.step(
            &format!("{GATE_HEAD_MOVED}{attempt} {head}"),
            "gate-head-moved",
            Some(attempt),
        )
    }

    /// A head that moved is gated again as the next attempt, which spends a retry: with none
    /// left the run is stuck.
    fn after_moved(&self, attempt: u32, log: &Log) -> Result<GateStep, RunError> {
        if u64::from(attempt - log.base_attempt) > self.retries() {
            return self.stuck("head-kept-moving", Stop::new(HEAD_KEPT_MOVING));
        }
        Ok(GateStep::NotReady)
    }

    /// The pull request is not the run's own, or is gone: the stuck step, or `None`.
    fn unbound(
        &self,
        seen: &Seen,
        pull: Option<&PullRequest>,
    ) -> Result<Option<GateStep>, RunError> {
        let branches: Vec<&str> = seen
            .last_branch
            .iter()
            .chain(&seen.resumed_from)
            .chain(&seen.launch_resumed_from)
            .chain(&seen.bound_branches)
            .map(String::as_str)
            .collect();
        let Some(unbound) = check_binding(pull, self.pr, self.base(seen), &branches) else {
            return Ok(None);
        };
        let stop = Stop {
            reason: unbound.public,
            detail: unbound.detail,
            ..Stop::default()
        };
        Ok(Some(self.stuck("pr-not-bound", stop)?))
    }

    // ----- a failed attempt -----

    /// A pull request someone took out of draft goes back to draft, by `building`'s rule for
    /// what GitHub's answer means. `Some` is the step the pass ends with instead: stuck when
    /// GitHub answered and the pull request is still not a draft, or gone, and ended when it
    /// was merged or closed meanwhile. A rate limit, a timeout, a refused login, `gh` not
    /// starting, an answer that cannot be read, a second look that fails, and a `gh` failure
    /// that got no answer from GitHub are an error with nothing more written, asked again
    /// next pass.
    fn back_to_draft(&self, pull: &PullRequest) -> Result<Option<GateStep>, RunError> {
        if !pull.open || pull.is_draft {
            return Ok(None);
        }
        let failed = match self.ctx.mark_pr_draft(self.pr) {
            Ok(()) => None,
            Err(
                error @ RunError::Tracker(
                    TrackerError::NotFound { .. }
                    | TrackerError::Other {
                        kind: OtherKind::Failed | OtherKind::NoAnswer,
                        ..
                    },
                ),
            ) => Some(error),
            Err(error) => return Err(error),
        };
        match self.ctx.tracker.pull_request(self.ctx.repo(), self.pr) {
            Err(read) => Err(failed.unwrap_or_else(|| read.into())),
            Ok(Some(now)) if !now.open => {
                self.archive_all()?;
                let (seen, _) = self.fresh()?;
                let mut at = self.at(&seen);
                at.worktree = None;
                self.end_session(true)?;
                end_run(self.ctx, &at, &self.url, Some(&now), CauseStep::Gate)?;
                Ok(Some(GateStep::Ended))
            }
            Ok(Some(now)) if failed.is_none() || now.is_draft => {
                self.ctx.record(
                    self.run,
                    Event::PrSetDraft { pr: self.pr },
                    &self.cause("pr-set-draft"),
                )?;
                Ok(None)
            }
            Ok(found) => {
                let no_answer = |error: &RunError| {
                    matches!(
                        error,
                        RunError::Tracker(TrackerError::Other {
                            kind: OtherKind::NoAnswer,
                            ..
                        })
                    )
                };
                if let Some(error) = failed.filter(no_answer) {
                    return Err(error);
                }
                let what = if found.is_some() {
                    "it is not a draft"
                } else {
                    "it does not exist"
                };
                let stop = Stop::new(reported_but(self.pr, what));
                Ok(Some(self.stuck("pr-not-bound", stop)?))
            }
        }
    }

    /// Attempt `attempt` failed with `kind`: back to draft, then sent back while a retry is
    /// left and stuck when none is.
    fn fail_attempt(
        &self,
        attempt: u32,
        kind: Kind,
        pull: Option<PullRequest>,
        turn: Option<u64>,
    ) -> Result<GateStep, RunError> {
        let (seen, log) = self.fresh()?;
        let pull = match pull {
            Some(pull) => pull,
            None => self
                .ctx
                .tracker
                .pull_request(self.ctx.repo(), self.pr)?
                .ok_or_else(|| RunError::Invalid("a bound pull request is gone".into()))?,
        };
        if let Some(step) = self.back_to_draft(&pull)? {
            return Ok(step);
        }
        let since = attempt - log.base_attempt;
        if u64::from(since) > self.retries() {
            let note = self.config_note(&seen, &log, attempt, kind, &pull)?;
            let (code, mut stop) = self.used_up(&seen, &log, attempt, kind, since)?;
            if note {
                stop.reason = format!("{}. {CONFIG_DIFFERS}", stop.reason);
            }
            return self.stuck(code, stop);
        }
        self.send_back(attempt, kind, turn)
    }

    /// The stuck path of an attempt that failed with no retry left.
    fn used_up(
        &self,
        seen: &Seen,
        log: &Log,
        attempt: u32,
        kind: Kind,
        since: u32,
    ) -> Result<(&'static str, Stop), RunError> {
        let failed_after = format!("the gate failed after {}", attempts(since));
        match kind {
            Kind::Command => {
                let (failure, how) = self.command_failure(log, attempt)?;
                let text = messages::failure_text(&failure);
                Ok((
                    "gate-retries-used-up",
                    Stop {
                        reason: format!(
                            "{failed_after}: {} failed ({how})",
                            gate::command_code(&failure.command)
                        ),
                        detail: Some(format!("{failed_after}\n\n{}", prepare_output_tail(&text))),
                        ..Stop::default()
                    },
                ))
            }
            Kind::Conflict => {
                let base = self.base(seen).to_owned();
                let text = messages::merge_conflict_text(&base, &[]);
                Ok((
                    "gate-retries-used-up",
                    Stop {
                        reason: format!(
                            "{failed_after}: the branch conflicts with the base branch {}",
                            sanitize(&base, false)
                        ),
                        detail: Some(format!("{failed_after}\n\n{}", prepare_output_tail(&text))),
                        ..Stop::default()
                    },
                ))
            }
            Kind::Checks => {
                let failed = seen.checks_failed.clone().ok_or_else(|| {
                    RunError::Invalid("the failed checks are not on record".into())
                })?;
                let list = self.failed_checks(&failed.sha, &failed.failed);
                let text = messages::checks_failure_text(&failed.sha, &list);
                let prepared = prepare_output_tail(&text);
                Ok((
                    "checks-retries-used-up",
                    Stop {
                        reason: format!("the required checks failed after {}", attempts(since)),
                        session_text: (!prepared.is_empty()).then_some(prepared),
                        ..Stop::default()
                    },
                ))
            }
        }
    }

    /// The failing command of `attempt`'s gate as the gate's tail names it. With the tail
    /// naming none, the failure is the gate's commands as a whole and the output the whole tail.
    /// Both are read as [`Self::read_tail`] hides them.
    fn command_failure(
        &self,
        log: &Log,
        attempt: u32,
    ) -> Result<(CommandFailure, String), RunError> {
        let req = log
            .reqs
            .iter()
            .rev()
            .find(|req| req.attempt == attempt && req.ended.is_some())
            .ok_or_else(|| RunError::Invalid("the gate's verdict is not on record".into()))?;
        let (code, tail) = req.ended.clone().unwrap_or_default();
        match self.read_tail(code, &tail)? {
            (
                GateResult::Failed {
                    command,
                    how,
                    output,
                },
                commands,
            ) => Ok((
                CommandFailure {
                    command: command.unwrap_or_else(|| commands.join(" && ")),
                    ended: gate::ended(code),
                    output,
                    truncated: false,
                },
                how,
            )),
            _ => Err(RunError::Invalid("the gate did not fail".into())),
        }
    }

    /// The verdict of a gate that ended with `code` and left `tail`, and the commands it was
    /// read against. The tail and each command are first hidden by the tracker's hider, with
    /// the secrets the machine holds now: the ledger hid the tail when it was written, maybe
    /// before a secret was held, and hiding a hidden text changes nothing. A command that holds
    /// a secret is then found in the tail as the `***` the tail has in its place, and no secret
    /// reaches the failure's command or its output.
    ///
    /// A store that cannot be read is an error, and nothing is read: the hider then gives its
    /// placeholder for every text, and a verdict read from that would be recorded and sent for
    /// good with no command and no output in it. The next pass reads the verdict again.
    fn read_tail(&self, code: i32, tail: &str) -> Result<(GateResult, Vec<String>), RunError> {
        let hide = |text: &str| {
            let hidden = self.ctx.tracker.hide(text);
            if hidden == crate::secrets::HIDDEN_UNREADABLE && text != hidden {
                return Err(RunError::Invalid(
                    "the gate's verdict waits: the secret store could not be read".into(),
                ));
            }
            Ok(hidden)
        };
        let commands = self
            .commands()
            .iter()
            .map(|command| hide(command))
            .collect::<Result<Vec<_>, _>>()?;
        let tail = hide(tail)?;
        let result = gate::result(TerminalState::Exited, Some(code), &tail, &commands);
        Ok((result, commands))
    }

    /// The failed checks as the message names them: what is on record, with the result and link
    /// of the ones GitHub still shows failed.
    fn failed_checks(&self, sha: &str, recorded: &[CheckStatus]) -> Vec<FailedCheck> {
        let names: Vec<String> = recorded.iter().map(|check| check.name.clone()).collect();
        let live = self
            .ctx
            .tracker
            .checks(self.ctx.repo(), sha, &names)
            .unwrap_or_default();
        recorded
            .iter()
            .map(|check| {
                let now = live
                    .iter()
                    .find(|live| live.name == check.name && live.state == CheckState::Failed);
                let shown = now.unwrap_or(check);
                FailedCheck {
                    name: shown.name.clone(),
                    conclusion: shown.conclusion.clone(),
                    url: shown.url.clone(),
                }
            })
            .collect()
    }

    /// Whether the `.dw/config.yml` at the commit the failure is about differs from the default
    /// branch's.
    fn config_note(
        &self,
        seen: &Seen,
        log: &Log,
        attempt: u32,
        kind: Kind,
        pull: &PullRequest,
    ) -> Result<bool, RunError> {
        let sha = match kind {
            Kind::Command => log
                .reqs
                .iter()
                .rev()
                .find(|req| req.attempt == attempt)
                .map(|req| req.sha.clone())
                .unwrap_or_else(|| pull.head_sha.to_lowercase()),
            Kind::Checks => seen
                .checks_failed
                .as_ref()
                .map_or_else(|| pull.head_sha.to_lowercase(), |failed| failed.sha.clone()),
            Kind::Conflict => pull.head_sha.to_lowercase(),
        };
        let tracker = self.ctx.tracker;
        let repo = self.ctx.repo();
        let default = tracker.default_branch(repo)?;
        let tip = tracker.branch_tip(repo, &default)?;
        let default_text = tracker.file_at(repo, config::PATH, &tip)?;
        let at_head = match tracker.file_at(repo, config::PATH, &sha) {
            Ok(text) => Some(text),
            Err(TrackerError::NotFound { .. }) => None,
            Err(error) => return Err(error.into()),
        };
        Ok(gate::config_differs(&default_text, at_head.as_deref()))
    }

    /// The failure sent into the session once, under the turn it was decided on.
    fn send_back(&self, attempt: u32, kind: Kind, turn: Option<u64>) -> Result<GateStep, RunError> {
        let (seen, log) = self.fresh()?;
        // A session this pass ended, or gave up on, has nothing to take the failure.
        if log.session_done() {
            return self.stuck("session-ended", Stop::new(SESSION_ENDED));
        }
        // One whose process ended cannot take it either: the interruption rule is `resume`'s.
        if !self
            .ctx
            .view
            .session_state(self.session)
            .is_some_and(can_take_a_message)
        {
            return Ok(GateStep::NotReady);
        }
        let ordinal = match seen.sending {
            Some(sending) if sending.attempt == attempt => sending.run_ordinal,
            _ => {
                let turn = turn.ok_or_else(|| {
                    RunError::Invalid("the session has no turn to send under".into())
                })?;
                u32::try_from(turn).unwrap_or(u32::MAX)
            }
        };
        let pull = self
            .ctx
            .tracker
            .pull_request(self.ctx.repo(), self.pr)?
            .ok_or_else(|| RunError::Invalid("a bound pull request is gone".into()))?;
        let mut message = match kind {
            Kind::Command => {
                let (failure, _) = self.command_failure(&log, attempt)?;
                messages::gate_failed(&GateFailure::Command(failure), BODY_PATH)
            }
            Kind::Conflict => messages::gate_failed(
                &GateFailure::Conflict {
                    base: self.base(&seen).to_owned(),
                    paths: vec![],
                },
                BODY_PATH,
            ),
            Kind::Checks => {
                let failed = seen.checks_failed.clone().ok_or_else(|| {
                    RunError::Invalid("the failed checks are not on record".into())
                })?;
                let list = self.failed_checks(&failed.sha, &failed.failed);
                messages::check_failed(&failed.sha, &list, BODY_PATH)
            }
        };
        if self.config_note(&seen, &log, attempt, kind, &pull)? {
            message = Message::new(
                message.line,
                format!("{}\n\n{CONFIG_DIFFERS}", message.body.unwrap_or_default()),
            );
        }
        let sending = seen
            .sending
            .is_some_and(|sending| sending.attempt == attempt && sending.run_ordinal == ordinal);
        if !sending {
            self.step(
                &format!("{GATE_FAILING}{attempt} {}", kind.name()),
                "gate-failing",
                Some(attempt),
            )?;
            self.step(
                &format!("{} {attempt} {ordinal}", GATE_SENDING),
                "gate-sending",
                Some(attempt),
            )?;
        }
        let (seen, _) = self.fresh()?;
        let mut at = self.at(&seen);
        let (_, actions) = transition(
            State::Building,
            &LifecycleEvent::GateFailed {
                output: message.body.clone().unwrap_or_default(),
            },
        )?;
        if let Some(workspace) = self.build_workspace(&seen)? {
            self.ctx.wake(workspace)?;
        }
        apply_actions(
            self.ctx,
            self.issue,
            &mut at,
            &actions,
            &self.cause("gate-failed"),
            &ActionPayload {
                send: Some(SendPayload {
                    message,
                    attempt,
                    run_ordinal: ordinal,
                }),
                ..ActionPayload::default()
            },
        )?;
        Ok(GateStep::SentBack { attempt })
    }

    // ----- a pass on record -----

    /// The pass is recorded: the required checks are waited for, then the pull request is made
    /// ready.
    fn after_pass(
        &self,
        seen: &Seen,
        log: &Log,
        pass: &PassRecord,
        state: Option<SessionState>,
        turn: Option<u64>,
    ) -> Result<GateStep, RunError> {
        let required = &self.ctx.config.required_checks;
        let wanted = !required.is_empty() && seen.checks_passed.as_deref() != Some(&pass.sha);
        let pull = self.ctx.tracker.pull_request(self.ctx.repo(), self.pr)?;
        // The binding is not looked at again after a recorded pass, as in the runner (gating.ts,
        // `gate`); only a moved head is gated again (the spec's one departure here).
        let Some(pull) = pull.filter(|pull| pull.open) else {
            return self.finish_pass(pass, false);
        };
        // Nothing is finished on a head that was not gated, whoever marked the pull request
        // ready: it goes back to draft, the move is recorded and the new head is gated.
        let head = pull.head_sha.to_lowercase();
        if head != pass.sha {
            if !pull.is_draft
                && let Some(step) = self.back_to_draft(&pull)?
            {
                return Ok(step);
            }
            self.moved(pass.attempt, &head, log)?;
            return self.after_moved(pass.attempt, log);
        }
        if !wanted {
            return self.finish_pass(pass, true);
        }
        let attempt = pass.attempt;
        let minutes = self.ctx.config.limits.checks_minutes;
        let timed_out = |missing: bool| {
            if missing {
                format!("a required check was not reported within {minutes} minutes")
            } else {
                format!("the required checks did not finish within {minutes} minutes")
            }
        };
        // A decision on record stands.
        if seen
            .checks_failed
            .as_ref()
            .is_some_and(|failed| failed.sha == pass.sha)
        {
            return self.checks_failed(pass, &pull, state, turn, None);
        }
        if let Some(timed) = seen
            .checks_timed_out
            .as_ref()
            .filter(|timed| timed.sha == pass.sha)
        {
            return self.stuck("checks-timed-out", Stop::new(timed_out(timed.missing)));
        }
        let checks = self
            .ctx
            .tracker
            .checks(self.ctx.repo(), &pass.sha, required)?;
        let read = Event::ChecksRead {
            sha: pass.sha.clone(),
            checks: checks
                .iter()
                .map(|check| CheckRead {
                    name: check.name.clone(),
                    state: match check.state {
                        CheckState::Passed => "passed",
                        CheckState::Failed => "failed",
                        CheckState::Pending => "pending",
                        CheckState::Missing => "missing",
                    }
                    .to_owned(),
                })
                .collect(),
        };
        if checks.iter().any(|check| check.state == CheckState::Failed) {
            return self.checks_failed(pass, &pull, state, turn, Some(read));
        }
        if checks.iter().all(|check| check.state == CheckState::Passed) {
            self.record(read, "required-checks-read", attempt)?;
            return self.finish_pass(pass, true);
        }
        // The wait is counted on the sleep-counting clock (`waited`).
        let waited = self.waited(log.checks_wait, CHECKS_WAITING, Some(attempt))?;
        if waited < Duration::from_secs(minutes.saturating_mul(60)) {
            return Ok(GateStep::ChecksPending);
        }
        let missing = checks
            .iter()
            .any(|check| check.state == CheckState::Missing);
        self.record(read, CHECKS_TIMED_OUT, attempt)?;
        self.stuck("checks-timed-out", Stop::new(timed_out(missing)))
    }

    /// A required check failed on the pass: it goes back to the session once the session can
    /// take it.
    fn checks_failed(
        &self,
        pass: &PassRecord,
        pull: &PullRequest,
        state: Option<SessionState>,
        turn: Option<u64>,
        read: Option<Event>,
    ) -> Result<GateStep, RunError> {
        let Some(state) = state else {
            return self.stuck("session-gone", Stop::new(SESSION_GONE));
        };
        if !can_take_a_message(state) || turn.is_none() {
            return Ok(GateStep::NotReady);
        }
        if let Some(read) = read {
            self.record(read, "required-checks-failed", pass.attempt)?;
        }
        self.fail_attempt(pass.attempt, Kind::Checks, Some(pull.clone()), turn)
    }

    /// The rest of a passing gate, in this order: every gate workspace archived or left, the
    /// pull request marked ready, then the build session ended (never before the ready mark,
    /// so a pass undone before it still has a session to go back to), the results posted, the
    /// label moved, and the build workspace archived or left. A pull request that is no
    /// longer open gets nothing marked or posted. `Passed` once all of it is done; otherwise
    /// the step the pass ends with, and the next pass carries on.
    fn finish_pass(&self, pass: &PassRecord, still_open: bool) -> Result<GateStep, RunError> {
        if let Some(step) = self.sweep_for_pr_open()? {
            return Ok(step);
        }
        let (seen, log) = self.fresh()?;
        let (_, actions) = transition(State::Building, &LifecycleEvent::PrReady { pr: self.pr })?;
        let cause = self.cause(GATE_PASSED).attempt(pass.attempt);
        // The ready mark first: until it holds, the pass can still be undone, and the session
        // it would have to go back to must be alive. A pull request that is no longer open
        // gets nothing marked or posted, and one marked ready by this pass is not marked
        // again by a pass that carries the finish on (a person's draft since then stands).
        if still_open && !log.ready_marked.contains(&pass.attempt) {
            let mut at = self.at(&seen);
            let mark = [Action::MarkPrReady { pr: self.pr }];
            apply_actions(
                self.ctx,
                self.issue,
                &mut at,
                &mark,
                &cause,
                &ActionPayload::default(),
            )?;
            self.step(
                &format!("{GATE_READY_MARKED}{}", pass.attempt),
                "gate-ready-marked",
                Some(pass.attempt),
            )?;
        }
        if !self.end_session(false)? {
            return Ok(GateStep::NotReady);
        }
        let (seen, _) = self.fresh()?;
        let mut at = self.at(&seen);
        let rest: Vec<Action> = actions
            .into_iter()
            .filter(|action| match action {
                Action::MarkPrReady { .. } => false,
                Action::PostGateResults { .. } => still_open,
                _ => true,
            })
            .collect();
        let count = usize::try_from(pass.count).unwrap_or(0);
        let results: Vec<CommandResult> = pass
            .lines
            .iter()
            .skip(pass.lines.len().saturating_sub(count))
            .map(|line| CommandResult {
                command: line.command.clone(),
                exit_code: line.exit_code,
                duration_ms: line.duration_ms,
            })
            .collect();
        let comment = gate::gate_results_comment(&results, pass.attempt, &pass.sha, &pass.base_sha);
        apply_actions(
            self.ctx,
            self.issue,
            &mut at,
            &rest,
            &cause,
            &ActionPayload {
                comment: Some(comment),
                ..ActionPayload::default()
            },
        )?;
        if let Some(step) = self.archive_build()? {
            return Ok(step);
        }
        Ok(GateStep::Passed { pr: self.pr })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::Duration;

    use foundry_proto::factory::{MachineHeld, SessionTurn};
    use foundry_proto::ids::{MachineId, SessionId, TerminalId, WorkspaceId};
    use foundry_proto::record::{MachineSample, Record, Session, Terminal, Workspace};
    use foundry_proto::settings::{Limits as MachineLimits, Machine};
    use foundry_proto::state::{SessionState, TerminalState};
    use serde_json::{Value, json};

    use super::super::testing::*;
    use super::*;
    use crate::factory::gate;
    use crate::factory::github::{Hiding, OtherKind, Tracker, TrackerError};
    use crate::factory::ledger::events::{Event, ResumedFrom};
    use crate::factory::lifecycle::State;
    use crate::factory::messages::{
        self, CommandFailure, FailedCheck, GateFailure, SESSION_GONE, STOPPED_BY_HAND,
    };
    use crate::factory::worker::{
        AlwaysHeld, Call, FleetView, FleetViewBuilder, HostCall, Lost, Target, WorkerError,
    };
    use crate::sessions::factory::{BODY_PATH, FactoryStart, SendRefused};
    use crate::workspaces::{Checkout, Setup};

    const SESSION: &str = "00000000-0000-0000-0000-0000000005e5";
    const WORKSPACE: &str = "00000000-0000-0000-0000-000000000a11";
    const BRANCH: &str = "fac/8-1";
    const URL12: &str = "https://github.com/rawphp/softfac/pull/12";
    const HEAD: &str = SHA;
    const MOVED: &str = "89abcdef0123456789abcdef0123456789abcdef";
    const BASE_TIP: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const DEFAULT_CONFIG: &str = "factory:\n  base: main\n  gate:\n    - cargo test\n";
    const PASS_TAIL: &str = "$ cargo test\nrunning 3 tests\n";
    const FAIL_OUTPUT: &str = "thread panicked: boom\n";
    const FAIL_TAIL: &str =
        "$ cargo test\nthread panicked: boom\nsetup failed: cargo test exited 101\n";

    fn session() -> SessionId {
        SessionId(uuid::Uuid::from_u128(0x5e5))
    }

    fn workspace() -> WorkspaceId {
        WorkspaceId(uuid::Uuid::from_u128(0xa11))
    }

    fn machine() -> MachineId {
        MachineId(uuid::Uuid::from_u128(0x2))
    }

    /// The machine the build session is on: online, and not one a gate can be placed on.
    fn builder() -> MachineId {
        MachineId(uuid::Uuid::from_u128(0x3))
    }

    fn gate_ws(k: u32) -> WorkspaceId {
        WorkspaceId(uuid::Uuid::from_u128(0xb00 + u128::from(k)))
    }

    fn gate_term(k: u32) -> TerminalId {
        TerminalId(uuid::Uuid::from_u128(0x7e0 + u128::from(k)))
    }

    // ----- GitHub: the simulator, with the reads of a gate -----

    #[derive(Clone)]
    struct HubPull {
        state: &'static str,
        draft: bool,
        head_ref: String,
        base: String,
        head: String,
        mergeable: &'static str,
        cross_repository: bool,
    }

    #[derive(Default)]
    struct HubState {
        pulls: BTreeMap<u64, HubPull>,
        checks: BTreeMap<String, Vec<(String, String, Option<String>)>>,
        tips: BTreeMap<String, String>,
        files: BTreeMap<String, String>,
        failures: Vec<(String, usize)>,
        reads: Vec<String>,
    }

    /// GitHub as the gate reads it: pull requests, check runs, branch tips and files that a
    /// test moves, over the simulator for everything else (issue, labels, comments).
    struct Hub {
        sim: Arc<Sim>,
        state: Mutex<HubState>,
    }

    fn encode64(text: &str) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in text.as_bytes().chunks(3) {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(TABLE[((n >> (18 - 6 * i)) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    impl Hub {
        fn lock(&self) -> std::sync::MutexGuard<'_, HubState> {
            self.state.lock().unwrap_or_else(PoisonError::into_inner)
        }

        fn set_pull(&self, number: u64, pull: HubPull) {
            self.lock().pulls.insert(number, pull);
        }

        fn edit(&self, number: u64, change: impl FnOnce(&mut HubPull)) {
            change(self.lock().pulls.get_mut(&number).expect("a pull request"));
        }

        fn set_head(&self, head: &str) {
            self.edit(12, |pull| pull.head = head.into());
        }

        fn set_state(&self, state: &'static str) {
            self.edit(12, |pull| pull.state = state);
        }

        fn set_draft(&self, draft: bool) {
            self.edit(12, |pull| pull.draft = draft);
        }

        fn set_mergeable(&self, mergeable: &'static str) {
            self.edit(12, |pull| pull.mergeable = mergeable);
        }

        fn remove_pull(&self) {
            self.lock().pulls.remove(&12);
        }

        fn set_checks(&self, sha: &str, runs: &[(&str, &str, Option<&str>)]) {
            self.lock().checks.insert(
                sha.into(),
                runs.iter()
                    .map(|(name, status, conclusion)| {
                        ((*name).into(), (*status).into(), conclusion.map(Into::into))
                    })
                    .collect(),
            );
        }

        fn set_file(&self, sha: &str, text: &str) {
            self.lock().files.insert(sha.into(), text.into());
        }

        fn set_tip(&self, branch: &str, sha: &str) {
            self.lock().tips.insert(branch.into(), sha.into());
        }

        /// The next `times` reads whose arguments hold `needle` fail.
        fn fail(&self, needle: &str, times: usize) {
            self.lock().failures.push((needle.into(), times));
        }

        /// Every read the hub answered, as one line each.
        fn reads(&self) -> Vec<String> {
            self.lock().reads.clone()
        }

        fn failed(args: &[String]) -> TrackerError {
            TrackerError::Other {
                kind: OtherKind::Failed,
                message: format!("`gh {}` failed: injected failure", args.join(" ")),
            }
        }

        fn not_found(args: &[String]) -> TrackerError {
            TrackerError::NotFound {
                command: format!("gh {}", args.join(" ")),
                status: 1,
                stderr: "HTTP 404: Not Found".into(),
            }
        }
    }

    impl Tracker for Hub {
        fn gh(&self, args: &[String]) -> Result<String, TrackerError> {
            let joined = args.join(" ");
            let word = |at: usize| args.get(at).map(String::as_str);
            let intercepted = matches!(
                (word(0), word(1)),
                (Some("pr"), Some("view")) | (Some("api"), Some(_))
            ) && !joined.contains("graphql")
                && !joined.contains("/issues/");
            let mut state = self.lock();
            if intercepted {
                state.reads.push(joined.clone());
                if let Some(at) = state
                    .failures
                    .iter()
                    .position(|(needle, times)| *times > 0 && joined.contains(needle.as_str()))
                {
                    state.failures[at].1 -= 1;
                    return Err(Self::failed(args));
                }
            }
            match (word(0), word(1)) {
                (Some("pr"), Some("view")) => {
                    let number: u64 = word(2).and_then(|n| n.parse().ok()).unwrap_or(0);
                    drop(state);
                    let Some(pull) = self.lock().pulls.get(&number).cloned() else {
                        return Err(TrackerError::Other {
                            kind: OtherKind::Failed,
                            message: "`gh pr view` failed: Could not resolve to a PullRequest"
                                .into(),
                        });
                    };
                    Ok(json!({
                        "number": number,
                        "isDraft": pull.draft,
                        "state": pull.state,
                        "headRefName": pull.head_ref,
                        "isCrossRepository": pull.cross_repository,
                        "baseRefName": pull.base,
                        "headRefOid": pull.head,
                        "mergeable": pull.mergeable,
                        "createdAt": LABELED_AT,
                    })
                    .to_string())
                }
                (Some("pr"), Some("ready")) => {
                    drop(state);
                    let result = self.sim.gh(args);
                    let landed = result.is_ok()
                        || result
                            .as_ref()
                            .is_err_and(|error| error.to_string().contains("answer lost"));
                    if landed {
                        let number: u64 = word(2).and_then(|n| n.parse().ok()).unwrap_or(0);
                        let undo = args.iter().any(|arg| arg == "--undo");
                        if let Some(pull) = self.lock().pulls.get_mut(&number) {
                            pull.draft = undo;
                        }
                    }
                    result
                }
                (Some("api"), Some(path)) if path.contains("/check-runs") => {
                    let sha = path
                        .split("/commits/")
                        .nth(1)
                        .and_then(|rest| rest.split('/').next())
                        .unwrap_or_default();
                    let runs: Vec<Value> = state
                        .checks
                        .get(sha)
                        .map(|runs| {
                            runs.iter()
                                .map(|(name, status, conclusion)| {
                                    json!({"name": name, "status": status,
                                        "conclusion": conclusion, "html_url": null})
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    Ok(Value::Array(runs).to_string())
                }
                (Some("api"), Some(path)) if path.contains("/git/ref/heads/") => {
                    let branch = path.split("/git/ref/heads/").nth(1).unwrap_or_default();
                    match state.tips.get(branch) {
                        Some(sha) => {
                            Ok(json!({"object": {"sha": sha, "type": "commit"}}).to_string())
                        }
                        None => Err(Self::not_found(args)),
                    }
                }
                (Some("api"), Some(path)) if path.contains("/contents/") => {
                    let sha = path.split("?ref=").nth(1).unwrap_or_default();
                    match state.files.get(sha) {
                        Some(text) => Ok(json!({
                            "type": "file",
                            "encoding": "base64",
                            "size": text.len(),
                            "content": encode64(text),
                        })
                        .to_string()),
                        None => Err(Self::not_found(args)),
                    }
                }
                (Some("api"), Some(path)) if *path == format!("repos/{REPO}") => {
                    Ok(json!({"default_branch": "main"}).to_string())
                }
                _ => {
                    drop(state);
                    self.sim.gh(args)
                }
            }
        }
    }

    // ----- the fleet and the run -----

    fn draft_on(head: &str) -> HubPull {
        HubPull {
            state: "OPEN",
            draft: true,
            head_ref: BRANCH.into(),
            base: "main".into(),
            head: head.into(),
            mergeable: "MERGEABLE",
            cross_repository: false,
        }
    }

    /// One gate workspace as the fleet shows it.
    #[derive(Clone, Copy)]
    struct G {
        k: u32,
        state: TerminalState,
        code: Option<i32>,
    }

    fn running(k: u32) -> G {
        G {
            k,
            state: TerminalState::Running,
            code: None,
        }
    }

    fn exited(k: u32, code: i32) -> G {
        G {
            k,
            state: TerminalState::Exited,
            code: Some(code),
        }
    }

    fn session_record(state: SessionState) -> Record<Session> {
        Record {
            owner: builder(),
            version: 1,
            deleted: false,
            body: Session {
                id: session(),
                workspace: workspace(),
                terminal: TerminalId(uuid::Uuid::from_u128(0x5e6)),
                model: "opus".into(),
                state,
                name: None,
            },
        }
    }

    fn gate_records(run: &RunId, gates: &[G]) -> Vec<(Record<Workspace>, Record<Terminal>)> {
        gates
            .iter()
            .map(|g| {
                let setup = gate_term(g.k);
                let workspace = Record {
                    owner: machine(),
                    version: 1,
                    deleted: false,
                    body: Workspace {
                        id: gate_ws(g.k),
                        project: project_id(),
                        name: gate::name(run, g.k),
                        branch: String::new(),
                        base: "main".into(),
                        setup: Some(setup),
                    },
                };
                let terminal = Record {
                    owner: machine(),
                    version: 1,
                    deleted: false,
                    body: Terminal {
                        id: setup,
                        workspace: gate_ws(g.k),
                        state: g.state,
                        exit_code: g.code,
                    },
                };
                (workspace, terminal)
            })
            .collect()
    }

    /// The machine a gate is placed on: online, holding the login, with room.
    fn gate_machine(view: FleetViewBuilder) -> FleetViewBuilder {
        view.online(machine())
            .online(builder())
            .sample(Record {
                owner: machine(),
                version: 3,
                deleted: false,
                body: MachineSample {
                    load_per_core: 0.1,
                    free_memory: 8_000_000_000,
                    free_disk: 100_000_000_000,
                    sessions: 0,
                    update_refused: None,
                    version: None,
                    update_ready: None,
                },
            })
            .held(
                machine(),
                MachineHeld {
                    logins: vec![LOGIN.into()],
                    skill: true,
                },
            )
    }

    /// The fleet shows the session in `state` with the newest turn `turn` (and as many prompts),
    /// and the gate workspaces `gates`. `turn` 0 shows no turn record.
    fn show(rig: &mut Rig, run: &RunId, state: SessionState, turn: u64, gates: &[G]) {
        show_prompts(rig, run, state, turn, turn, false, gates);
    }

    /// As [`show`], the session having had `prompts` prompts by its turn `turn`, and a question
    /// waiting when `question`.
    fn show_prompts(
        rig: &mut Rig,
        run: &RunId,
        state: SessionState,
        turn: u64,
        prompts: u64,
        question: bool,
        gates: &[G],
    ) {
        let mut view = gate_machine(FleetView::builder()).session(session_record(state));
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
        for (workspace, terminal) in gate_records(run, gates) {
            view = view.workspace(workspace).terminal(terminal);
        }
        rig.view = view.build();
    }

    /// As [`show`], with the gate's machine offline for `span`.
    fn show_gone(rig: &mut Rig, run: &RunId, turn: u64, gates: &[G], span: Duration) {
        show_gone_with(rig, run, SessionState::Idle, turn, gates, span);
    }

    /// As [`show_gone`], with the session shown `state`.
    fn show_gone_with(
        rig: &mut Rig,
        run: &RunId,
        state: SessionState,
        turn: u64,
        gates: &[G],
        span: Duration,
    ) {
        let mut view = FleetView::builder()
            .online(builder())
            .session(session_record(state))
            .turn(SessionTurn {
                session: session(),
                workspace: workspace(),
                project: project_id(),
                turn,
                prompts: turn,
                question: false,
            })
            .unreachable(machine(), span);
        for (workspace, terminal) in gate_records(run, gates) {
            view = view.workspace(workspace).terminal(terminal);
        }
        rig.view = view.build();
    }

    fn shared() -> foundry_proto::settings::SharedSettings {
        foundry_proto::settings::SharedSettings {
            machines: vec![Machine {
                id: machine(),
                name: "vps".into(),
                address: String::new(),
                key: String::new(),
            }],
            limits: BTreeMap::from([(
                machine(),
                MachineLimits {
                    max_sessions: 4,
                    max_load_per_core: 1.0,
                    min_free_disk: 10_000_000_000,
                },
            )]),
            ..foundry_proto::settings::SharedSettings::default()
        }
    }

    fn hub(rig: &Rig) -> Arc<Hub> {
        Arc::new(Hub {
            sim: rig.sim.clone(),
            state: Mutex::new(HubState::default()),
        })
    }

    /// The reads a gate makes, answered: the base's tip, the default branch's config.
    fn install(rig: &mut Rig) -> Arc<Hub> {
        let hub = hub(rig);
        hub.set_pull(12, draft_on(HEAD));
        hub.set_tip("main", BASE_TIP);
        hub.set_file(BASE_TIP, DEFAULT_CONFIG);
        hub.set_file(HEAD, DEFAULT_CONFIG);
        rig.tracker = Hiding::new(hub.clone(), Arc::new(|text: &str| text.to_string()));
        hub
    }

    /// A building run whose session reported pull request #12, an open draft on its branch, and
    /// is idle after turn 1. The fleet has a machine a gate can be placed on.
    fn built() -> (Rig, RunId, Arc<Hub>) {
        built_on(Rig::new())
    }

    fn built_on(rig: Rig) -> (Rig, RunId, Arc<Hub>) {
        built_launch(rig, Some(BRANCH), None, Some("main"))
    }

    /// As `built`, the session launched on `branch`, resumed from the branch `resumed` and on
    /// the base `base`, as the launch line records them.
    fn built_launch(
        mut rig: Rig,
        branch: Option<&str>,
        resumed: Option<&str>,
        base: Option<&str>,
    ) -> (Rig, RunId, Arc<Hub>) {
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
        rig.step(&run, &format!("workspace-created {WORKSPACE}"));
        rig.step(&run, "approval-rechecked");
        rig.write(
            &run,
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
            },
        );
        rig.moved(&run, State::Triage, State::Building);
        rig.write(
            &run,
            Event::PrOpened {
                pr: 12,
                url: URL12.into(),
            },
        );
        rig.step(&run, "turn-read 1");
        rig.sim.set_labels(&["factory:building"]);
        rig.sim.set_assignees(&[LOGIN]);
        let hub = install(&mut rig);
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
                messages::reminder(),
            )
            .unwrap();
        show(&mut rig, &run, SessionState::Idle, 1, &[]);
        (rig, run, hub)
    }

    /// The session reported its pull request again after turn `turn`, as `building` records it.
    fn reported_again(rig: &mut Rig, run: &RunId, hub: &Hub, turn: u64) {
        rig.write(
            run,
            Event::PrOpened {
                pr: 12,
                url: URL12.into(),
            },
        );
        rig.step(run, &format!("turn-read {turn}"));
        hub.set_draft(true);
        show(rig, run, SessionState::Idle, turn, &[]);
    }

    fn open_slots() -> Slots {
        Slots {
            runs_free: 1,
            gates_free: 1,
        }
    }

    fn try_go(rig: &Rig, run: &RunId) -> Result<GateStep, RunError> {
        advance(&rig.ctx(), run, &shared(), &mut open_slots())
    }

    fn go(rig: &Rig, run: &RunId) -> GateStep {
        try_go(rig, run).unwrap()
    }

    /// One pass that places the gate `k`: the worker's create answers with its workspace.
    fn place(rig: &mut Rig, run: &RunId, turn: u64, k: u32) {
        rig.worker.on_create(Ok(gate_ws(k)));
        assert_eq!(go(rig, run), GateStep::Running);
        show(rig, run, SessionState::Idle, turn, &[running(k)]);
    }

    /// The gate `k` ends with exit code `code` and `tail`, and one pass reads it.
    fn ends(rig: &mut Rig, run: &RunId, turn: u64, k: u32, code: i32, tail: &str) -> GateStep {
        show(rig, run, SessionState::Idle, turn, &[exited(k, code)]);
        rig.worker.on_tail(Ok(tail.into()));
        go(rig, run)
    }

    /// The first gate of a run at turn 1, placed and ended.
    fn gated(rig: &mut Rig, run: &RunId, code: i32, tail: &str) -> GateStep {
        place(rig, run, 1, 1);
        ends(rig, run, 1, 1, code, tail)
    }

    fn calls(rig: &Rig) -> Vec<Call> {
        rig.worker
            .calls()
            .into_iter()
            .filter(|call| !matches!(call, Call::StartSession { .. }))
            .collect()
    }

    fn archived(rig: &Rig) -> Vec<WorkspaceId> {
        calls(rig)
            .into_iter()
            .filter_map(|call| match call {
                Call::Archive { workspace } => Some(workspace),
                _ => None,
            })
            .collect()
    }

    fn creates(rig: &Rig) -> usize {
        calls(rig)
            .iter()
            .filter(|call| matches!(call, Call::Create { .. }))
            .count()
    }

    fn sends(rig: &Rig) -> Vec<Call> {
        calls(rig)
            .into_iter()
            .filter(|call| matches!(call, Call::Send { .. }))
            .collect()
    }

    /// The host calls of the worker, without how many calls came before each.
    fn host_calls(rig: &Rig) -> Vec<HostCall> {
        rig.worker
            .host_calls()
            .into_iter()
            .map(|(_, call)| call)
            .collect()
    }

    /// The build workspace was woken exactly once, and before the first send.
    fn woke_before_the_send(rig: &Rig) {
        let woken: Vec<usize> = rig
            .worker
            .host_calls()
            .into_iter()
            .filter(|(_, call)| {
                *call
                    == HostCall::Wake {
                        workspace: workspace(),
                    }
            })
            .map(|(before, _)| before)
            .collect();
        let send = rig
            .worker
            .calls()
            .iter()
            .position(|call| matches!(call, Call::Send { .. }))
            .expect("a send");
        assert_eq!(woken.len(), 1, "{:?}", rig.worker.host_calls());
        assert!(
            woken[0] <= send,
            "woke after call {} but sent at {send}",
            woken[0]
        );
    }

    fn archive_of(workspace: WorkspaceId) -> Call {
        Call::Archive { workspace }
    }

    fn end_of_build() -> Call {
        Call::EndSessions {
            workspace: workspace(),
        }
    }

    fn no_writes(rig: &Rig) {
        assert_eq!(rig.sim.writes(), Vec::<String>::new());
    }

    fn event_count(rig: &Rig, run: &RunId, name: &str) -> usize {
        rig.events(run)
            .iter()
            .filter(|event| event.event == name)
            .count()
    }

    fn step_count(rig: &Rig, run: &RunId, prefix: &str) -> usize {
        rig.steps(run)
            .iter()
            .filter(|step| step.starts_with(prefix))
            .count()
    }

    fn has_step(rig: &Rig, run: &RunId, prefix: &str) -> bool {
        step_count(rig, run, prefix) > 0
    }

    fn written_at(writes: &[String], needle: &str) -> usize {
        writes
            .iter()
            .position(|write| write.contains(needle))
            .unwrap_or_else(|| panic!("no write holding {needle:?} in {writes:?}"))
    }

    fn failure_of(command: &str, code: i32, output: &str) -> GateFailure {
        GateFailure::Command(CommandFailure {
            command: command.into(),
            ended: gate::ended(code),
            output: output.into(),
            truncated: false,
        })
    }

    /// The message a failed gate sends: the runner's failure text over the tail's output.
    fn failed_message(output: &str) -> messages::Message {
        messages::gate_failed(&failure_of("cargo test", 101, output), BODY_PATH)
    }

    fn sent(prompts: u64, message: messages::Message) -> Call {
        Call::Send {
            session: session(),
            message,
            only_if_prompts: Some(prompts),
        }
    }

    /// The run went stuck under `code`: the label, the assignee, the comment and the summary
    /// line, the session ended and the build workspace kept.
    #[track_caller]
    fn assert_stuck(rig: &Rig, run: &RunId, step: &GateStep, code: &str, public: &str) {
        assert_stuck_in(rig, run, step, code, public, false);
    }

    /// As `assert_stuck`; `build_archived_before` is a run whose build workspace an earlier round
    /// archived at its pass, which is the one thing that then differs.
    #[track_caller]
    fn assert_stuck_in(
        rig: &Rig,
        run: &RunId,
        step: &GateStep,
        code: &str,
        public: &str,
        build_archived_before: bool,
    ) {
        let GateStep::Stuck { reason } = step else {
            panic!("expected a stuck run, got {step:?}");
        };
        let comment = rig
            .sim
            .comments()
            .into_iter()
            .find(|comment| comment.contains("could not finish"))
            .expect("a stuck comment");
        assert!(comment.contains(&format!("Reason: {public}")), "{comment}");
        let events = rig.events(run);
        let line = events
            .iter()
            .find(|event| event.event == "stuck")
            .expect("a stuck line");
        assert_eq!(line.data["reason"].as_str(), Some(reason.as_str()));
        let last = events.last().unwrap();
        assert_eq!(last.event, "finished");
        assert_eq!(last.data["outcome"], "stuck");
        assert_eq!(last.cause.as_ref().unwrap()["reason"], code);
        assert_eq!(rig.sim.labels(), ["factory:stuck"]);
        assert!(rig.sim.assignees().is_empty());
        assert!(calls(rig).contains(&end_of_build()));
        let archives_of_build = calls(rig)
            .iter()
            .filter(|call| **call == archive_of(workspace()))
            .count();
        // Kept for a person to look at; round one's pass archived it once already.
        assert_eq!(archives_of_build, usize::from(build_archived_before));
    }

    // ----- the runner's `gate: the session's PR becomes ready, or it does not` -----

    mod gate_the_sessions_pr_becomes_ready_or_it_does_not {
        use super::*;

        fn the_gate_request(rig: &Rig, run: &RunId) -> Call {
            Call::Create {
                target: Target::Machine(machine()),
                request: gate::request(&rig.config, project_id(), HEAD, run, 1),
            }
        }

        #[test]
        fn the_build_workspace_is_put_at_rest_before_the_gate_is_placed() {
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            let rested = rig.worker.host_calls();
            let create = rig
                .worker
                .calls()
                .iter()
                .position(|call| matches!(call, Call::Create { .. }))
                .expect("a create");
            assert_eq!(
                rested,
                [(
                    create,
                    HostCall::Rest {
                        workspace: workspace()
                    }
                )]
            );
            assert_eq!(creates(&rig), 1);
        }

        #[test]
        fn a_session_that_is_not_ready_for_a_gate_is_not_put_at_rest() {
            let (mut rig, run, _hub) = built();
            show(&mut rig, &run, SessionState::Working, 1, &[]);
            assert_eq!(go(&rig, &run), GateStep::NotReady);
            assert!(rig.worker.host_calls().is_empty());
        }

        #[test]
        fn a_gate_whose_terminal_runs_is_advanced_with_the_gates_workspace() {
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            assert!(
                !host_calls(&rig)
                    .iter()
                    .any(|call| matches!(call, HostCall::Advance { .. }))
            );
            assert_eq!(go(&rig, &run), GateStep::Running);
            assert!(host_calls(&rig).contains(&HostCall::Advance {
                workspace: gate_ws(1)
            }));
        }

        #[test]
        fn a_gate_whose_terminal_exited_is_not_advanced() {
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            assert_eq!(
                ends(&mut rig, &run, 1, 1, 0, PASS_TAIL),
                GateStep::Passed { pr: 12 }
            );
            assert!(
                !host_calls(&rig)
                    .iter()
                    .any(|call| matches!(call, HostCall::Advance { .. }))
            );
        }

        #[test]
        fn a_gate_that_passes_marks_the_pr_ready_posts_the_results_and_moves_the_issue_to_pr_open()
        {
            let (mut rig, run, hub) = built();
            assert_eq!(
                gated(&mut rig, &run, 0, PASS_TAIL),
                GateStep::Passed { pr: 12 }
            );
            let writes = rig.sim.writes();
            assert!(
                written_at(&writes, "pr ready 12") < written_at(&writes, "pr comment 12"),
                "{writes:?}"
            );
            assert!(
                written_at(&writes, "pr comment 12") < written_at(&writes, "factory:pr-open"),
                "{writes:?}"
            );
            assert!(
                rig.sim
                    .comments()
                    .iter()
                    .any(|c| c.contains("**Factory gate: passed** (attempt 1")),
                "{:?}",
                rig.sim.comments()
            );
            assert_eq!(rig.sim.labels(), ["factory:pr-open"]);
            assert!(!hub.lock().pulls[&12].draft);
            assert_eq!(
                calls(&rig),
                vec![
                    the_gate_request(&rig, &run),
                    Call::Tail {
                        terminal: gate_term(1)
                    },
                    archive_of(gate_ws(1)),
                    end_of_build(),
                    archive_of(workspace()),
                ]
            );
            assert!(has_step(
                &rig,
                &run,
                &format!("{GATE_REQUESTED}1 1 {HEAD} {BASE_TIP}")
            ));
            assert!(has_step(&rig, &run, "gate-passed "));
            assert_eq!(event_count(&rig, &run, "gate_checkout"), 1);
            // The label moves before the build workspace is archived, so the state change is
            // recorded, and the archive's own steps follow it.
            assert!(
                rig.events(&run)
                    .iter()
                    .any(|event| event.event == "state_changed" && event.data["to"] == "pr-open")
            );
        }

        #[test]
        fn a_gate_pass_archives_the_session_and_removes_its_worktree_before_the_issue_is_pr_open() {
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            // The session cannot be ended: the pull request was marked ready first, nothing else
            // is written, the label stays, and the next pass ends it and goes on.
            rig.worker
                .on_end_sessions(Err(WorkerError::Failed("the machine is busy".into())));
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 0)]);
            rig.worker.on_tail(Ok(PASS_TAIL.into()));
            assert!(matches!(try_go(&rig, &run), Err(RunError::Worker(_))));
            assert_eq!(rig.sim.writes().len(), 1);
            assert!(rig.sim.writes()[0].contains("pr ready 12"));
            assert_eq!(rig.sim.labels(), ["factory:building"]);
            assert!(!calls(&rig).contains(&archive_of(workspace())));
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
            assert_eq!(rig.sim.labels(), ["factory:pr-open"]);
            let tails = calls(&rig)
                .iter()
                .filter(|call| matches!(call, Call::Tail { .. }))
                .count();
            assert_eq!(tails, 1, "the gate's tail is read once");
            let ended = calls(&rig)
                .iter()
                .filter(|call| **call == end_of_build())
                .count();
            assert_eq!(ended, 2, "the failed call and the one that went through");
        }

        #[test]
        fn a_worktree_that_cannot_be_removed_at_the_pass_is_recorded_and_the_issue_still_moves_on()
        {
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 0)]);
            rig.worker.on_tail(Ok(PASS_TAIL.into()));
            rig.worker.on_archive(Ok(())); // the gate's
            rig.worker
                .on_archive(Err(WorkerError::Failed("the worktree is busy".into())));
            assert!(matches!(try_go(&rig, &run), Err(RunError::Worker(_))));
            // The label moved first: what is left is the archive, asked again next pass.
            assert_eq!(rig.sim.labels(), ["factory:pr-open"]);
            let writes = rig.sim.writes().len();
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
            assert_eq!(rig.sim.writes().len(), writes, "nothing is written twice");
            let archives_of_build = calls(&rig)
                .iter()
                .filter(|call| **call == archive_of(workspace()))
                .count();
            assert_eq!(archives_of_build, 2);
            assert_eq!(step_count(&rig, &run, BUILD_ARCHIVED), 1);
            assert_eq!(
                calls(&rig)
                    .iter()
                    .filter(|call| **call == end_of_build())
                    .count(),
                1
            );
        }

        #[test]
        fn the_gate_runs_the_commands_of_the_project_config_the_caller_passed() {
            let (mut rig, run, hub) = built();
            rig.config.provision = vec!["npm ci".into()];
            rig.config.gate = vec!["cargo test".into(), "cargo clippy".into()];
            // The pull request's own copy says otherwise: the gate never reads it.
            hub.set_file(HEAD, "factory:\n  gate:\n    - true\n");
            place(&mut rig, &run, 1, 1);
            let request = gate::request(&rig.config, project_id(), HEAD, &run, 1);
            let Call::Create { request: asked, .. } = &calls(&rig)[0] else {
                panic!("expected a create");
            };
            assert_eq!(asked, &request);
            assert_eq!(asked.name, "fac/8-1-g1");
            assert_eq!(asked.checkout, Checkout::Commit { sha: HEAD.into() });
            let Setup::Commands { commands, .. } = &asked.setup else {
                panic!("expected commands");
            };
            assert_eq!(commands, &["npm ci", "cargo test", "cargo clippy"]);
            assert!(!hub.reads().iter().any(|read| read.contains("contents")));
        }

        #[test]
        fn the_results_comment_of_a_pass_found_in_the_ledger_names_the_base_it_ran_against() {
            let (mut rig, run, hub) = built();
            // The base moves after the gate was placed: the results name the tip it ran against.
            place(&mut rig, &run, 1, 1);
            hub.set_tip("main", MOVED);
            assert_eq!(
                ends(&mut rig, &run, 1, 1, 0, PASS_TAIL),
                GateStep::Passed { pr: 12 }
            );
            let comment = rig
                .sim
                .comments()
                .into_iter()
                .find(|comment| comment.contains("Factory gate: passed"))
                .expect("the results");
            assert!(comment.contains("whose tip was `aaaaaaa`"), "{comment}");
        }

        #[test]
        fn a_passed_gate_is_not_run_again_once_the_issue_is_pr_open() {
            let (mut rig, run, hub) = built();
            gated(&mut rig, &run, 0, PASS_TAIL);
            let before = (
                rig.events(&run).len(),
                rig.worker.calls().len(),
                rig.sim.calls().len(),
                hub.reads().len(),
            );
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
            assert_eq!(
                before,
                (
                    rig.events(&run).len(),
                    rig.worker.calls().len(),
                    rig.sim.calls().len(),
                    hub.reads().len()
                )
            );
        }

        #[test]
        fn gate_does_nothing_for_a_run_that_is_not_ready_for_it() {
            // A run with no events at all.
            let (rig, run, _hub) = built();
            let other = RunId::new(REPO, 9, 1).unwrap();
            assert_eq!(go(&rig, &other), GateStep::NotReady);
            // Finished.
            rig.ledger
                .finish(
                    &run,
                    crate::factory::ledger::events::Outcome::Stuck,
                    None,
                    None,
                    &crate::factory::ledger::events::Cause::new(
                        crate::factory::ledger::events::CauseStep::Gate,
                        "test",
                    ),
                    None,
                )
                .unwrap();
            let before = rig.events(&run).len();
            assert_eq!(go(&rig, &run), GateStep::NotReady);
            assert_eq!(rig.events(&run).len(), before);
            no_writes(&rig);
            assert_eq!(calls(&rig), vec![]);
        }

        #[test]
        fn gate_does_nothing_for_a_run_whose_session_reported_no_pull_request() {
            let rig = Rig::new();
            let run = run_id(1);
            rig.step(&run, &format!("approved {LABELED_AT}"));
            rig.moved(&run, State::Ready, State::Triage);
            rig.moved(&run, State::Triage, State::Building);
            assert_eq!(go(&rig, &run), GateStep::NotReady);
            no_writes(&rig);
            assert_eq!(calls(&rig), vec![]);
        }

        #[test]
        fn gate_does_nothing_while_the_sessions_run_is_still_going() {
            for state in [
                SessionState::Starting,
                SessionState::Working,
                SessionState::WaitingForYou,
            ] {
                let (mut rig, run, hub) = built();
                show(&mut rig, &run, state, 1, &[]);
                let before = rig.events(&run).len();
                assert_eq!(go(&rig, &run), GateStep::NotReady, "{state:?}");
                no_writes(&rig);
                assert_eq!(calls(&rig), vec![], "{state:?}");
                assert_eq!(rig.events(&run).len(), before, "{state:?}");
                assert_eq!(hub.reads(), Vec::<String>::new(), "{state:?}");
            }
        }

        #[test]
        fn the_reported_pull_request_of_a_session_that_cannot_be_reached_is_gated() {
            // Its last turn stands: the gate needs the pull request, not the session. A message
            // sent earlier and shown in the prompt count does not hold it back.
            let cases: [(&str, u64, &[&str]); 3] = [
                ("its first turn read", 1, &[]),
                (
                    "a message refused with nothing typed",
                    1,
                    &[
                        "send-intent forward-comment c1 1",
                        "send-refused forward-comment c1",
                    ],
                ),
                (
                    "a message sent earlier and shown",
                    2,
                    &[
                        "send-intent forward-comment c1 1",
                        "send-typed forward-comment c1",
                        "turn-read 2",
                    ],
                ),
            ];
            for (case, turn, steps) in cases {
                let (mut rig, run, _hub) = built();
                for step in steps {
                    rig.step(&run, step);
                }
                show(&mut rig, &run, SessionState::Unreachable, turn, &[]);
                rig.worker.on_create(Ok(gate_ws(1)));
                assert_eq!(go(&rig, &run), GateStep::Running, "{case}");
                assert!(
                    has_step(&rig, &run, &format!("{GATE_REQUESTED}1 1 {HEAD}")),
                    "{case}: {:?}",
                    rig.steps(&run)
                );
            }
        }

        /// The fleet shows a session on a machine it cannot reach as it last saw it. One that may
        /// be at work there is waited for, as one shown working is, so its work is not passed by.
        #[test]
        fn the_pull_request_of_a_session_that_cannot_be_reached_and_may_be_at_work_is_not_gated() {
            let cases: [(&str, u64, u64, bool, Option<&str>); 4] = [
                ("a prompt after its newest turn", 1, 2, false, None),
                ("a newer turn not read", 2, 2, false, None),
                ("a question waiting", 1, 1, true, None),
                (
                    "a message sent and not shown yet",
                    1,
                    1,
                    false,
                    Some("send-intent forward-comment c1 1"),
                ),
            ];
            for (case, turn, prompts, question, step) in cases {
                let (mut rig, run, hub) = built();
                if let Some(step) = step {
                    rig.step(&run, step);
                }
                show_prompts(
                    &mut rig,
                    &run,
                    SessionState::Unreachable,
                    turn,
                    prompts,
                    question,
                    &[],
                );
                let before = rig.events(&run).len();
                assert_eq!(go(&rig, &run), GateStep::NotReady, "{case}");
                no_writes(&rig);
                assert_eq!(calls(&rig), vec![], "{case}");
                assert_eq!(rig.events(&run).len(), before, "{case}");
                assert_eq!(hub.reads(), Vec::<String>::new(), "{case}");
            }
        }

        #[test]
        fn a_failure_found_while_the_session_cannot_be_reached_waits_for_it_and_is_sent_once() {
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            show(
                &mut rig,
                &run,
                SessionState::Unreachable,
                1,
                &[exited(1, 101)],
            );
            rig.worker.on_tail(Ok(FAIL_TAIL.into()));
            assert_eq!(go(&rig, &run), GateStep::NotReady);
            // The gate was read while the session was away: its verdict is on record.
            assert_eq!(event_count(&rig, &run, "gate_finished"), 1);
            assert_eq!(go(&rig, &run), GateStep::NotReady);
            assert_eq!(sends(&rig), vec![]);
            assert_eq!(step_count(&rig, &run, "send-intent"), 0);
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 101)]);
            assert_eq!(go(&rig, &run), GateStep::SentBack { attempt: 1 });
            assert_eq!(sends(&rig).len(), 1);
            assert_eq!(rig.ctx().seen(&run).unwrap().sent_back_attempts.len(), 1);
        }

        #[test]
        fn a_merged_or_closed_pr_found_before_the_gate_runs_ends_the_run_and_nothing_is_gated() {
            for state in ["MERGED", "CLOSED"] {
                let (rig, run, hub) = built();
                hub.set_state(state);
                assert_eq!(go(&rig, &run), GateStep::Ended, "{state}");
                assert_eq!(creates(&rig), 0);
                no_writes(&rig);
                assert!(rig.finished(&run));
                let last = rig.events(&run).pop().unwrap();
                assert_eq!(
                    last.data["pr_end"],
                    if state == "MERGED" {
                        "merged"
                    } else {
                        "closed"
                    }
                );
                assert!(calls(&rig).contains(&end_of_build()));
            }
        }

        #[test]
        fn a_pr_merged_while_the_gate_commands_ran_records_the_pass_and_ends_the_run_as_merged() {
            let (mut rig, run, hub) = built();
            place(&mut rig, &run, 1, 1);
            hub.set_state("MERGED");
            assert_eq!(ends(&mut rig, &run, 1, 1, 0, PASS_TAIL), GateStep::Ended);
            assert!(has_step(&rig, &run, "gate-passed "));
            // Nothing is marked ready or posted on a pull request that is merged.
            assert!(
                !rig.sim
                    .writes()
                    .iter()
                    .any(|write| write.contains("pr ready") || write.contains("pr comment"))
            );
            assert!(rig.finished(&run));
            assert_eq!(rig.events(&run).pop().unwrap().data["pr_end"], "merged");
        }

        #[test]
        fn a_pr_merged_with_a_new_head_while_the_gate_commands_ran_gets_no_pass_and_ends_next_call()
        {
            let (mut rig, run, hub) = built();
            place(&mut rig, &run, 1, 1);
            hub.set_state("MERGED");
            hub.set_head(MOVED);
            assert_eq!(ends(&mut rig, &run, 1, 1, 0, PASS_TAIL), GateStep::NotReady);
            assert!(!has_step(&rig, &run, "gate-passed "));
            assert_eq!(go(&rig, &run), GateStep::Ended);
            assert_eq!(
                creates(&rig),
                1,
                "no second gate is placed on a merged pull request"
            );
            assert!(rig.finished(&run));
        }

        #[test]
        fn a_pr_closed_while_the_gate_commands_ran_ends_the_run_as_closed_without_a_pass() {
            let (mut rig, run, hub) = built();
            place(&mut rig, &run, 1, 1);
            hub.set_state("CLOSED");
            assert_eq!(ends(&mut rig, &run, 1, 1, 0, PASS_TAIL), GateStep::Ended);
            assert!(!has_step(&rig, &run, "gate-passed "));
            no_writes(&rig);
            assert_eq!(rig.events(&run).pop().unwrap().data["pr_end"], "closed");
        }

        #[test]
        fn a_pr_marked_ready_outside_while_the_gate_ran_still_passes_and_is_noted_without_who() {
            let (mut rig, run, hub) = built();
            place(&mut rig, &run, 1, 1);
            hub.set_draft(false);
            assert_eq!(
                ends(&mut rig, &run, 1, 1, 0, PASS_TAIL),
                GateStep::Passed { pr: 12 }
            );
            let noted: Vec<_> = rig
                .events(&run)
                .into_iter()
                .filter(|event| event.event == "pr_ready_outside")
                .collect();
            assert_eq!(noted.len(), 1);
            assert_eq!(noted[0].data["pr"], 12);
            assert!(noted[0].data.get("by").is_none());
        }

        #[test]
        fn a_pr_marked_ready_outside_is_noted_once_for_the_run() {
            let (mut rig, run, hub) = built();
            rig.config.required_checks = vec!["ci".into()];
            hub.set_checks(HEAD, &[("ci", "completed", Some("failure"))]);
            hub.set_draft(false);
            place(&mut rig, &run, 1, 1);
            // The gate passes (noted), then the check fails and the failure goes back.
            assert_eq!(
                ends(&mut rig, &run, 1, 1, 0, PASS_TAIL),
                GateStep::SentBack { attempt: 1 }
            );
            assert_eq!(event_count(&rig, &run, "pr_ready_outside"), 1);
            reported_again(&mut rig, &run, &hub, 2);
            hub.set_checks(HEAD, &[("ci", "completed", Some("success"))]);
            hub.set_draft(false);
            place(&mut rig, &run, 2, 2);
            assert_eq!(
                ends(&mut rig, &run, 2, 2, 0, PASS_TAIL),
                GateStep::Passed { pr: 12 }
            );
            assert_eq!(event_count(&rig, &run, "pr_ready_outside"), 1);
        }

        #[test]
        fn a_pr_marked_ready_outside_whose_head_moved_is_no_pass_and_is_not_noted() {
            let (mut rig, run, hub) = built();
            place(&mut rig, &run, 1, 1);
            hub.set_draft(false);
            hub.set_head(MOVED);
            assert_eq!(ends(&mut rig, &run, 1, 1, 0, PASS_TAIL), GateStep::NotReady);
            assert!(!has_step(&rig, &run, "gate-passed "));
            assert_eq!(event_count(&rig, &run, "pr_ready_outside"), 0);
        }

        #[test]
        fn a_failed_gate_puts_a_pr_someone_else_marked_ready_back_to_draft_and_sends_it_back() {
            let (mut rig, run, hub) = built();
            place(&mut rig, &run, 1, 1);
            hub.set_draft(false);
            assert_eq!(
                ends(&mut rig, &run, 1, 1, 101, FAIL_TAIL),
                GateStep::SentBack { attempt: 1 }
            );
            assert!(hub.lock().pulls[&12].draft);
            assert_eq!(event_count(&rig, &run, "pr_set_draft"), 1);
            let writes = rig.sim.writes();
            assert!(
                writes
                    .iter()
                    .any(|w| w.contains("pr ready 12") && w.contains("--undo"))
            );
            assert_eq!(sends(&rig), vec![sent(1, failed_message(FAIL_OUTPUT))]);
        }

        #[test]
        fn a_failed_gate_whose_pr_cannot_be_put_back_to_draft_sends_the_issue_to_stuck() {
            let (mut rig, run, hub) = built();
            place(&mut rig, &run, 1, 1);
            hub.set_draft(false);
            rig.sim.fail("--undo", 2);
            let step = ends(&mut rig, &run, 1, 1, 101, FAIL_TAIL);
            assert_stuck(
                &rig,
                &run,
                &step,
                "pr-not-bound",
                "the session reported PR #12 but it is not a draft",
            );
            assert_eq!(sends(&rig), vec![]);
        }

        #[test]
        fn a_rate_limit_on_putting_the_pr_back_to_draft_after_a_failed_gate_is_an_error_and_not_stuck()
         {
            let (mut rig, run, hub) = built();
            place(&mut rig, &run, 1, 1);
            hub.set_draft(false);
            rig.tracker = Hiding::new(
                Arc::new(RateLimitedDraft(hub.clone())),
                Arc::new(|text: &str| text.to_string()),
            );
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 101)]);
            rig.worker.on_tail(Ok(FAIL_TAIL.into()));
            let error = try_go(&rig, &run).unwrap_err();
            assert!(matches!(
                error,
                RunError::Tracker(TrackerError::RateLimited { .. })
            ));
            assert!(!rig.finished(&run));
            assert_eq!(rig.sim.labels(), ["factory:building"]);
            assert_eq!(sends(&rig), vec![]);
        }

        struct RateLimitedDraft(Arc<Hub>);

        impl Tracker for RateLimitedDraft {
            fn gh(&self, args: &[String]) -> Result<String, TrackerError> {
                if args.iter().any(|arg| arg == "--undo") {
                    return Err(TrackerError::RateLimited {
                        command: "gh pr ready".into(),
                        stderr: "HTTP 429".into(),
                    });
                }
                self.0.gh(args)
            }
        }

        #[test]
        fn a_pr_that_is_gone_at_gate_time_sends_the_issue_to_stuck() {
            let (rig, run, hub) = built();
            hub.remove_pull();
            let step = go(&rig, &run);
            assert_stuck(
                &rig,
                &run,
                &step,
                "pr-not-bound",
                "the session reported PR #12 but it does not exist",
            );
            assert_eq!(creates(&rig), 0);
        }

        #[test]
        fn the_pr_is_checked_again_at_gate_time_and_a_changed_one_sends_the_issue_to_stuck() {
            let (mut rig, run, hub) = built();
            place(&mut rig, &run, 1, 1);
            hub.edit(12, |pull| pull.head_ref = "someone/else".into());
            let step = ends(&mut rig, &run, 1, 1, 0, PASS_TAIL);
            assert_stuck(
                &rig,
                &run,
                &step,
                "pr-not-bound",
                "the session reported PR #12 but its branch is not this session's branch",
            );
            assert!(!has_step(&rig, &run, "gate-passed "));
        }

        #[test]
        fn a_pr_on_another_base_sends_the_issue_to_stuck_before_it_is_gated() {
            let (rig, run, hub) = built();
            hub.edit(12, |pull| pull.base = "release".into());
            let step = go(&rig, &run);
            assert_stuck(
                &rig,
                &run,
                &step,
                "pr-not-bound",
                "the session reported PR #12 but its base is not the base branch main",
            );
            assert_eq!(creates(&rig), 0);
        }

        #[test]
        fn a_thread_that_is_gone_at_gate_time_sends_the_issue_to_stuck() {
            let (mut rig, run, _hub) = built();
            rig.view = gate_machine(FleetView::builder()).build();
            let step = go(&rig, &run);
            assert_stuck(&rig, &run, &step, "session-gone", SESSION_GONE);
            assert_eq!(creates(&rig), 0);
        }

        #[test]
        fn a_failing_pr_read_at_gate_time_is_an_error_and_writes_nothing() {
            let (rig, run, hub) = built();
            hub.fail("pr view", 1);
            let before = rig.events(&run).len();
            assert!(matches!(try_go(&rig, &run), Err(RunError::Tracker(_))));
            assert_eq!(rig.events(&run).len(), before);
            no_writes(&rig);
            assert_eq!(calls(&rig), vec![]);
            assert_eq!(go(&rig, &run), GateStep::Running);
        }

        #[test]
        fn gate_leaves_a_run_whose_pr_open_state_is_already_written_alone_without_touching_github()
        {
            let (rig, run, hub) = built();
            rig.step(&run, &format!("gate-passed {HEAD} 1"));
            rig.moved(&run, State::Building, State::PrOpen);
            rig.step(&run, BUILD_SESSION_ENDED);
            rig.step(&run, BUILD_ARCHIVING);
            rig.step(&run, BUILD_ARCHIVED);
            rig.sim.set_labels(&["factory:pr-open"]);
            let before = (rig.events(&run).len(), rig.sim.calls().len());
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
            assert_eq!(before, (rig.events(&run).len(), rig.sim.calls().len()));
            assert_eq!(hub.reads(), Vec::<String>::new());
            assert_eq!(calls(&rig), vec![]);
        }
    }

    // ----- the runner's `gate: a failed attempt goes back into the session` -----

    mod gate_a_failed_attempt_goes_back_into_the_session {
        use super::*;

        fn gate_sent_back_lines(rig: &Rig, run: &RunId) -> Vec<(u64, u64)> {
            rig.events(run)
                .iter()
                .filter(|event| event.event == "gate_sent_back")
                .map(|event| {
                    (
                        event.data["attempt"].as_u64().unwrap(),
                        event.data["run_ordinal"].as_u64().unwrap(),
                    )
                })
                .collect()
        }

        #[test]
        fn a_failed_attempt_wakes_the_build_workspace_before_it_is_sent_into_the_session() {
            let (mut rig, run, _hub) = built();
            assert_eq!(
                gated(&mut rig, &run, 101, FAIL_TAIL),
                GateStep::SentBack { attempt: 1 }
            );
            woke_before_the_send(&rig);
        }

        #[test]
        fn a_failed_attempt_is_sent_into_the_session_and_the_next_gate_waits_for_a_new_run() {
            let (mut rig, run, hub) = built();
            assert_eq!(
                gated(&mut rig, &run, 101, FAIL_TAIL),
                GateStep::SentBack { attempt: 1 }
            );
            assert_eq!(sends(&rig), vec![sent(1, failed_message(FAIL_OUTPUT))]);
            assert_eq!(gate_sent_back_lines(&rig, &run), [(1, 1)]);
            assert!(has_step(&rig, &run, "gate-sending 1 1"));
            // The gate's workspace is archived; the build session is not ended.
            assert!(calls(&rig).contains(&archive_of(gate_ws(1))));
            assert!(!calls(&rig).contains(&end_of_build()));
            assert_eq!(rig.sim.labels(), ["factory:building"]);
            // The session has not run again: nothing is gated.
            show(&mut rig, &run, SessionState::Idle, 1, &[]);
            assert_eq!(go(&rig, &run), GateStep::NotReady);
            show(&mut rig, &run, SessionState::Working, 2, &[]);
            assert_eq!(go(&rig, &run), GateStep::NotReady);
            assert_eq!(creates(&rig), 1);
            // Its next report is gated as the next attempt, in the next workspace.
            reported_again(&mut rig, &run, &hub, 2);
            rig.worker.on_create(Ok(gate_ws(2)));
            assert_eq!(go(&rig, &run), GateStep::Running);
            assert!(has_step(&rig, &run, &format!("{GATE_REQUESTED}2 2 {HEAD}")));
            let Call::Create { request, .. } = calls(&rig).pop().unwrap() else {
                panic!("expected a create");
            };
            assert_eq!(request.name, "fac/8-1-g2");
        }

        #[test]
        fn a_send_that_reached_the_session_is_recorded_once_its_next_run_ended_and_gated_as_the_next_attempt()
         {
            let (mut rig, run, hub) = built();
            place(&mut rig, &run, 1, 1);
            rig.worker.on_send(Err(SendRefused::Failed {
                reason: "the answer was lost".into(),
                prompts: Some(1),
            }));
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 101)]);
            rig.worker.on_tail(Ok(FAIL_TAIL.into()));
            assert!(matches!(
                try_go(&rig, &run),
                Err(RunError::Send(SendRefused::Failed { .. }))
            ));
            assert_eq!(gate_sent_back_lines(&rig, &run), []);
            // The line was typed: the session ran, and its count rose.
            rig.worker.on_send(Err(SendRefused::CountRose { now: 2 }));
            show(&mut rig, &run, SessionState::Idle, 2, &[]);
            assert_eq!(go(&rig, &run), GateStep::SentBack { attempt: 1 });
            assert_eq!(gate_sent_back_lines(&rig, &run), [(1, 1)]);
            assert_eq!(sends(&rig).len(), 2);
            reported_again(&mut rig, &run, &hub, 2);
            rig.worker.on_create(Ok(gate_ws(2)));
            assert_eq!(go(&rig, &run), GateStep::Running);
            assert!(has_step(&rig, &run, &format!("{GATE_REQUESTED}2 2 {HEAD}")));
        }

        #[test]
        fn the_second_failure_is_sent_with_its_own_command_id_and_the_ordinal_it_read() {
            let (mut rig, run, hub) = built();
            gated(&mut rig, &run, 101, FAIL_TAIL);
            reported_again(&mut rig, &run, &hub, 2);
            place(&mut rig, &run, 2, 2);
            assert_eq!(
                ends(&mut rig, &run, 2, 2, 101, FAIL_TAIL),
                GateStep::SentBack { attempt: 2 }
            );
            assert_eq!(gate_sent_back_lines(&rig, &run), [(1, 1), (2, 2)]);
            assert_eq!(
                sends(&rig),
                vec![
                    sent(1, failed_message(FAIL_OUTPUT)),
                    sent(2, failed_message(FAIL_OUTPUT))
                ]
            );
            assert!(has_step(&rig, &run, "send-typed gate-failure 1 1"));
            assert!(has_step(&rig, &run, "send-typed gate-failure 2 2"));
        }

        #[test]
        fn a_timed_out_command_counts_as_a_failure_and_the_message_says_so() {
            let (mut rig, run, _hub) = built();
            let tail = "$ cargo test\nsetup failed: cargo test ran past 1800 s and was stopped\n";
            assert_eq!(
                gated(&mut rig, &run, 124, tail),
                GateStep::SentBack { attempt: 1 }
            );
            let expected = messages::gate_failed(&failure_of("cargo test", 124, ""), BODY_PATH);
            assert_eq!(sends(&rig), vec![sent(1, expected.clone())]);
            assert!(expected.body.unwrap().contains("timed out after 1800 s"));
        }

        #[test]
        fn the_first_failing_command_is_the_one_sent_and_the_commands_after_it_never_ran() {
            let (mut rig, run, _hub) = built();
            rig.config.gate = vec!["cargo fmt".into(), "cargo test".into()];
            let tail = "$ cargo fmt\nDiff in main.rs\nsetup failed: cargo fmt exited 1\n";
            assert_eq!(
                gated(&mut rig, &run, 1, tail),
                GateStep::SentBack { attempt: 1 }
            );
            let expected =
                messages::gate_failed(&failure_of("cargo fmt", 1, "Diff in main.rs\n"), BODY_PATH);
            assert_eq!(sends(&rig), vec![sent(1, expected)]);
            let lines: Vec<_> = rig
                .events(&run)
                .into_iter()
                .filter(|event| event.event == "gate_command")
                .collect();
            assert_eq!(lines.len(), 1);
            assert_eq!(lines[0].data["command"], "cargo fmt");
            assert_eq!(lines[0].data["exit_code"], 1);
        }

        #[test]
        fn a_failed_gate_whose_tail_names_no_command_is_sent_as_the_gates_commands_as_a_whole() {
            let (mut rig, run, _hub) = built();
            rig.config.gate = vec!["cargo fmt".into(), "cargo test".into()];
            assert_eq!(
                gated(&mut rig, &run, 2, "garbage the code printed\n"),
                GateStep::SentBack { attempt: 1 }
            );
            let expected = messages::gate_failed(
                &failure_of("cargo fmt && cargo test", 2, "garbage the code printed\n"),
                BODY_PATH,
            );
            assert_eq!(sends(&rig), vec![sent(1, expected)]);
            let line = rig
                .events(&run)
                .into_iter()
                .find(|event| event.event == "gate_command")
                .expect("a command line");
            assert_eq!(line.data["command"], "cargo fmt && cargo test");
            assert_eq!(line.data["exit_code"], 2);
        }

        /// The secret the hiding of [`built_hiding`] knows, as the machine's store would hold a
        /// `github:*` token or a gateway key.
        const HELD: &str = "ghp_held0123456789";

        /// As `built`, with the ledger and the tracker hiding [`HELD`] as `***`, as the
        /// daemon's hider hides a secret the machine holds.
        fn built_hiding() -> (Rig, RunId, Arc<Hub>) {
            let hider = || Arc::new(|text: &str| text.replace(HELD, "***"));
            let mut rig = Rig::new();
            let (dir, store) = crate::factory::ledger::testing::open_store();
            rig.ledger = crate::factory::ledger::Ledger::new(
                store,
                MachineId(uuid::Uuid::from_u128(0x1)),
                1,
                hider(),
            )
            .with_wall(crate::factory::ledger::testing::ticking(
                crate::factory::ledger::testing::T0,
                1000,
            ));
            rig._dir = dir;
            let (mut rig, run, hub) = built_on(rig);
            rig.tracker = Hiding::new(hub.clone(), hider());
            (rig, run, hub)
        }

        #[test]
        fn a_failed_command_that_holds_a_known_secret_is_sent_with_the_secret_hidden() {
            let (mut rig, run, _hub) = built_hiding();
            let command = format!("cargo test --token {HELD}");
            rig.config.gate = vec![command.clone()];
            let tail =
                format!("$ {command}\nfailed with {HELD}\nsetup failed: {command} exited 101\n");
            assert_eq!(
                gated(&mut rig, &run, 101, &tail),
                GateStep::SentBack { attempt: 1 }
            );
            let expected = messages::gate_failed(
                &failure_of("cargo test --token ***", 101, "failed with ***\n"),
                BODY_PATH,
            );
            assert_eq!(sends(&rig), vec![sent(1, expected)]);
        }

        #[test]
        fn the_gates_commands_as_a_whole_are_sent_with_a_known_secret_hidden() {
            let (mut rig, run, _hub) = built_hiding();
            rig.config.gate = vec!["cargo fmt".into(), format!("cargo test --token {HELD}")];
            assert_eq!(
                gated(&mut rig, &run, 2, "garbage the code printed\n"),
                GateStep::SentBack { attempt: 1 }
            );
            let expected = messages::gate_failed(
                &failure_of(
                    "cargo fmt && cargo test --token ***",
                    2,
                    "garbage the code printed\n",
                ),
                BODY_PATH,
            );
            assert_eq!(sends(&rig), vec![sent(1, expected)]);
        }

        #[test]
        fn a_failed_command_with_no_secret_is_sent_unchanged_by_the_hiding() {
            let (mut rig, run, _hub) = built_hiding();
            rig.config.gate = vec!["cargo fmt".into(), format!("cargo test --token {HELD}")];
            let tail = "$ cargo fmt\nDiff in main.rs\nsetup failed: cargo fmt exited 1\n";
            assert_eq!(
                gated(&mut rig, &run, 1, tail),
                GateStep::SentBack { attempt: 1 }
            );
            let expected =
                messages::gate_failed(&failure_of("cargo fmt", 1, "Diff in main.rs\n"), BODY_PATH);
            assert_eq!(sends(&rig), vec![sent(1, expected)]);
        }

        #[test]
        fn the_failed_command_on_record_is_the_one_that_holds_the_secret_hidden() {
            let (mut rig, run, _hub) = built_hiding();
            let command = format!("cargo test --token {HELD}");
            rig.config.gate = vec!["cargo fmt".into(), command.clone()];
            let tail = format!("$ cargo fmt\n$ {command}\nsetup failed: {command} exited 101\n");
            assert_eq!(
                gated(&mut rig, &run, 101, &tail),
                GateStep::SentBack { attempt: 1 }
            );
            let lines: Vec<_> = rig
                .events(&run)
                .into_iter()
                .filter(|event| event.event == "gate_command")
                .collect();
            assert_eq!(lines.len(), 1);
            assert_eq!(lines[0].data["command"], "cargo test --token ***");
        }

        #[test]
        fn a_secret_the_machine_came_to_hold_after_the_tail_was_recorded_is_hidden_in_the_message()
        {
            // The ledger recorded the tail while the store did not hold the token yet.
            let (mut rig, run, hub) = built();
            rig.tracker = Hiding::new(
                hub.clone(),
                Arc::new(|text: &str| text.replace(HELD, "***")),
            );
            let command = format!("cargo test --token {HELD}");
            rig.config.gate = vec![command.clone()];
            let tail =
                format!("$ {command}\nfailed with {HELD}\nsetup failed: {command} exited 101\n");
            assert_eq!(
                gated(&mut rig, &run, 101, &tail),
                GateStep::SentBack { attempt: 1 }
            );
            let expected = messages::gate_failed(
                &failure_of("cargo test --token ***", 101, "failed with ***\n"),
                BODY_PATH,
            );
            assert_eq!(sends(&rig), vec![sent(1, expected)]);
        }

        #[test]
        fn a_gate_whose_retries_are_used_up_names_its_command_with_the_secret_hidden() {
            // The ledger recorded the tail while the store did not hold the token yet, so only
            // the hiding of the tail as it is read keeps the token out of the stuck detail.
            let (mut rig, run, hub) = built();
            rig.tracker = Hiding::new(
                hub.clone(),
                Arc::new(|text: &str| text.replace(HELD, "***")),
            );
            rig.config.limits.gate_retries = 0;
            let command = format!("cargo test --token {HELD}");
            rig.config.gate = vec![command.clone()];
            let tail =
                format!("$ {command}\nfailed with {HELD}\nsetup failed: {command} exited 101\n");
            let step = gated(&mut rig, &run, 101, &tail);
            let GateStep::Stuck { reason } = &step else {
                panic!("expected stuck, got {step:?}");
            };
            assert!(reason.contains("cargo test --token ***"), "{reason}");
            assert!(!reason.contains(HELD), "{reason}");
            let details: Vec<_> = rig
                .steps(&run)
                .into_iter()
                .filter(|step| step.starts_with("stuck-detail "))
                .collect();
            assert_eq!(details.len(), 1, "{details:?}");
            assert!(details[0].contains("failed with ***"), "{details:?}");
            assert!(!details[0].contains(HELD), "{details:?}");
        }

        #[test]
        fn a_passed_gate_read_while_the_secret_store_cannot_be_read_records_no_command() {
            let (mut rig, run, hub) = built();
            place(&mut rig, &run, 1, 1);
            rig.tracker = Hiding::new(
                hub.clone(),
                Arc::new(|_: &str| crate::secrets::HIDDEN_UNREADABLE.to_owned()),
            );
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 0)]);
            rig.worker.on_tail(Ok(PASS_TAIL.into()));
            assert!(try_go(&rig, &run).is_err());
            assert_eq!(event_count(&rig, &run, "gate_command"), 0);
            rig.tracker = Hiding::new(hub.clone(), Arc::new(|text: &str| text.to_owned()));
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
            let recorded: Vec<Value> = rig
                .events(&run)
                .into_iter()
                .filter(|event| event.event == "gate_command")
                .map(|event| event.data["command"].clone())
                .collect();
            let expected: Vec<Value> = rig.config.gate.iter().map(|c| json!(c)).collect();
            assert_eq!(recorded, expected);
        }

        #[test]
        fn a_gate_read_while_the_secret_store_cannot_be_read_records_and_sends_nothing() {
            let (mut rig, run, hub) = built();
            let command = format!("cargo test --token {HELD}");
            rig.config.gate = vec![command.clone()];
            let tail =
                format!("$ {command}\nfailed with {HELD}\nsetup failed: {command} exited 101\n");
            place(&mut rig, &run, 1, 1);
            // The store cannot be read: the hider gives its placeholder for every text.
            rig.tracker = Hiding::new(
                hub.clone(),
                Arc::new(|_: &str| crate::secrets::HIDDEN_UNREADABLE.to_owned()),
            );
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 101)]);
            rig.worker.on_tail(Ok(tail.clone()));
            assert!(try_go(&rig, &run).is_err());
            assert_eq!(event_count(&rig, &run, "gate_command"), 0);
            assert_eq!(sends(&rig), vec![]);
            // Once it can be read again, the same verdict is read whole.
            rig.tracker = Hiding::new(
                hub.clone(),
                Arc::new(|text: &str| text.replace(HELD, "***")),
            );
            assert_eq!(go(&rig, &run), GateStep::SentBack { attempt: 1 });
            let lines: Vec<_> = rig
                .events(&run)
                .into_iter()
                .filter(|event| event.event == "gate_command")
                .collect();
            assert_eq!(lines.len(), 1);
            assert_eq!(lines[0].data["command"], "cargo test --token ***");
            let expected = messages::gate_failed(
                &failure_of("cargo test --token ***", 101, "failed with ***\n"),
                BODY_PATH,
            );
            assert_eq!(sends(&rig), vec![sent(1, expected)]);
        }

        #[test]
        fn a_failed_provision_goes_back_into_the_session_like_a_failed_gate() {
            let (mut rig, run, _hub) = built();
            rig.config.provision = vec!["npm ci".into()];
            let tail = "$ npm ci\nnpm ERR! offline\nsetup failed: npm ci exited 1\n";
            assert_eq!(
                gated(&mut rig, &run, 1, tail),
                GateStep::SentBack { attempt: 1 }
            );
            let expected =
                messages::gate_failed(&failure_of("npm ci", 1, "npm ERR! offline\n"), BODY_PATH);
            assert_eq!(sends(&rig), vec![sent(1, expected)]);
        }

        #[test]
        fn a_send_that_fails_leaves_the_attempt_open_and_a_retry_sends_under_the_same_command_id() {
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            rig.worker.on_send(Err(SendRefused::Failed {
                reason: "the terminal did not answer".into(),
                prompts: Some(1),
            }));
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 101)]);
            rig.worker.on_tail(Ok(FAIL_TAIL.into()));
            assert!(matches!(try_go(&rig, &run), Err(RunError::Send(_))));
            assert_eq!(gate_sent_back_lines(&rig, &run), []);
            assert_eq!(go(&rig, &run), GateStep::SentBack { attempt: 1 });
            assert_eq!(gate_sent_back_lines(&rig, &run), [(1, 1)]);
            let message = failed_message(FAIL_OUTPUT);
            assert_eq!(
                sends(&rig),
                vec![sent(1, message.clone()), sent(1, message)]
            );
            assert_eq!(step_count(&rig, &run, "send-typed gate-failure 1 1"), 1);
            // The gate was read once: its tail is not read again.
            let tails = calls(&rig)
                .iter()
                .filter(|call| matches!(call, Call::Tail { .. }))
                .count();
            assert_eq!(tails, 1);
        }
    }

    // ----- the runner's `gate: the retries run out` -----

    mod gate_the_retries_run_out {
        use super::*;

        fn failed_after(attempts: usize) -> String {
            format!(
                "the gate failed after {attempts} attempt{}",
                if attempts == 1 { "" } else { "s" }
            )
        }

        fn recorded(attempts: usize, output: &str) -> String {
            let text = messages::failure_text(&CommandFailure {
                command: "cargo test".into(),
                ended: gate::ended(101),
                output: output.into(),
                truncated: false,
            });
            format!(
                "{}\n\n{}",
                failed_after(attempts),
                messages::prepare_output_tail(&text)
            )
        }

        #[test]
        fn gate_retries_is_how_many_failed_attempts_may_go_back_into_the_session() {
            let (mut rig, run, hub) = built();
            rig.config.limits.gate_retries = 1;
            assert_eq!(
                gated(&mut rig, &run, 101, FAIL_TAIL),
                GateStep::SentBack { attempt: 1 }
            );
            reported_again(&mut rig, &run, &hub, 2);
            place(&mut rig, &run, 2, 2);
            let step = ends(&mut rig, &run, 2, 2, 101, FAIL_TAIL);
            assert_stuck(
                &rig,
                &run,
                &step,
                "gate-retries-used-up",
                &format!("{}: `cargo test` failed (exit code 101)", failed_after(2)),
            );
            assert_eq!(
                step,
                GateStep::Stuck {
                    reason: recorded(2, FAIL_OUTPUT)
                }
            );
            assert_eq!(sends(&rig).len(), 1);
        }

        #[test]
        fn a_run_whose_retries_run_out_keeps_its_build_workspace_once_after_its_session_ends() {
            let (mut rig, run, _hub) = built();
            rig.config.limits.gate_retries = 0;
            let step = gated(&mut rig, &run, 101, FAIL_TAIL);
            assert!(matches!(step, GateStep::Stuck { .. }), "{step:?}");
            let kept: Vec<usize> = rig
                .worker
                .host_calls()
                .into_iter()
                .filter(|(_, call)| {
                    *call
                        == HostCall::Keep {
                            workspace: workspace(),
                        }
                })
                .map(|(before, _)| before)
                .collect();
            assert_eq!(kept.len(), 1, "{:?}", rig.worker.host_calls());
            let ended = rig
                .worker
                .calls()
                .iter()
                .position(|call| *call == end_of_build())
                .expect("the build session is ended");
            assert!(
                kept[0] > ended,
                "kept after call {} but ended at {ended}",
                kept[0]
            );
        }

        #[test]
        fn a_failed_attempt_is_stuck_at_once_when_no_retries_are_allowed() {
            let (mut rig, run, _hub) = built();
            rig.config.limits.gate_retries = 0;
            let step = gated(&mut rig, &run, 101, FAIL_TAIL);
            assert_stuck(
                &rig,
                &run,
                &step,
                "gate-retries-used-up",
                &format!("{}: `cargo test` failed (exit code 101)", failed_after(1)),
            );
            assert_eq!(sends(&rig), vec![]);
        }

        #[test]
        fn the_failing_output_is_kept_from_the_stuck_comment_and_prepared_for_the_ledger() {
            let (mut rig, run, _hub) = built();
            rig.config.limits.gate_retries = 0;
            let tail = "$ cargo test\nthe output names SECRET-OUTPUT\nsetup failed: cargo test exited 101\n";
            let step = gated(&mut rig, &run, 101, tail);
            let GateStep::Stuck { reason } = &step else {
                panic!("expected stuck, got {step:?}");
            };
            assert!(reason.contains("SECRET-OUTPUT"), "{reason}");
            assert_eq!(*reason, recorded(1, "the output names SECRET-OUTPUT\n"));
            assert!(
                rig.sim
                    .comments()
                    .iter()
                    .all(|comment| !comment.contains("SECRET-OUTPUT"))
            );
        }

        #[test]
        fn a_timed_out_command_is_named_in_the_stuck_comment_with_its_time_limit() {
            let (mut rig, run, _hub) = built();
            rig.config.limits.gate_retries = 0;
            let tail = "$ cargo test\nsetup failed: cargo test ran past 1800 s and was stopped\n";
            let step = gated(&mut rig, &run, 124, tail);
            assert_stuck(
                &rig,
                &run,
                &step,
                "gate-retries-used-up",
                &format!(
                    "{}: `cargo test` failed (ran longer than 30 minutes)",
                    failed_after(1)
                ),
            );
        }

        #[test]
        fn a_gate_failure_fixed_in_one_cycle_spends_one_retry() {
            let (mut rig, run, hub) = built();
            rig.config.limits.gate_retries = 1;
            assert_eq!(
                gated(&mut rig, &run, 101, FAIL_TAIL),
                GateStep::SentBack { attempt: 1 }
            );
            reported_again(&mut rig, &run, &hub, 2);
            place(&mut rig, &run, 2, 2);
            // One retry allowed and one spent: the fix passes.
            assert_eq!(
                ends(&mut rig, &run, 2, 2, 0, PASS_TAIL),
                GateStep::Passed { pr: 12 }
            );
            let attempts: Vec<u64> = rig
                .events(&run)
                .iter()
                .filter(|event| event.event == "gate_checkout")
                .map(|event| event.data["attempt"].as_u64().unwrap())
                .collect();
            assert_eq!(attempts, [1, 2]);
            assert!(calls(&rig).contains(&archive_of(gate_ws(1))));
            assert!(calls(&rig).contains(&archive_of(gate_ws(2))));
        }

        #[test]
        fn a_moved_head_spends_a_retry_like_a_failed_gate() {
            let (mut rig, run, hub) = built();
            rig.config.limits.gate_retries = 0;
            place(&mut rig, &run, 1, 1);
            hub.set_head(MOVED);
            let step = ends(&mut rig, &run, 1, 1, 0, PASS_TAIL);
            assert_stuck(&rig, &run, &step, "head-kept-moving", HEAD_KEPT_MOVING);
            assert_eq!(creates(&rig), 1);
            assert!(!has_step(&rig, &run, "gate-passed "));
        }

        fn sha(n: u32) -> String {
            format!("{n:040x}")
        }

        #[test]
        fn a_head_that_keeps_moving_ends_stuck() {
            let (mut rig, run, hub) = built();
            rig.config.limits.gate_retries = 2;
            let mut step = GateStep::NotReady;
            for k in 1..=3u32 {
                // The gate is placed on the head the pull request has, which then moves.
                rig.worker.on_create(Ok(gate_ws(k)));
                let placed = go(&rig, &run);
                assert_eq!(placed, GateStep::Running, "gate {k}");
                show(&mut rig, &run, SessionState::Idle, 1, &[running(k)]);
                hub.set_head(&sha(k + 100));
                step = ends(&mut rig, &run, 1, k, 0, PASS_TAIL);
                if k < 3 {
                    assert_eq!(step, GateStep::NotReady, "gate {k}");
                }
            }
            assert_stuck(&rig, &run, &step, "head-kept-moving", HEAD_KEPT_MOVING);
            assert_eq!(creates(&rig), 3, "no fourth gate is placed");
            assert!(!has_step(&rig, &run, "gate-passed "));
            // Each gate was placed on the head the pull request had then.
            let requested: Vec<String> = rig
                .steps(&run)
                .into_iter()
                .filter(|step| step.starts_with(GATE_REQUESTED))
                .collect();
            assert_eq!(requested.len(), 3);
            assert!(requested[1].contains(&sha(101)), "{requested:?}");
            assert!(requested[2].contains(&sha(102)), "{requested:?}");
        }
    }

    // ----- the runner's `gate: GitHub says the pull request conflicts with its base` -----

    mod gate_github_says_the_pull_request_conflicts_with_its_base {
        use super::*;

        fn conflict_message() -> messages::Message {
            messages::gate_failed(
                &GateFailure::Conflict {
                    base: "main".into(),
                    paths: vec![],
                },
                BODY_PATH,
            )
        }

        fn go_without_a_slot(rig: &Rig, run: &RunId) -> GateStep {
            let mut none = Slots {
                runs_free: 1,
                gates_free: 0,
            };
            advance(&rig.ctx(), run, &shared(), &mut none).unwrap()
        }

        #[test]
        fn a_conflict_wakes_the_build_workspace_before_it_is_sent() {
            let (rig, run, hub) = built();
            hub.set_mergeable("CONFLICTING");
            assert_eq!(
                go_without_a_slot(&rig, &run),
                GateStep::SentBack { attempt: 1 }
            );
            woke_before_the_send(&rig);
        }

        #[test]
        fn a_building_run_is_sent_the_conflict_without_waiting_for_a_gate_slot() {
            let (rig, run, hub) = built();
            hub.set_mergeable("CONFLICTING");
            assert_eq!(
                go_without_a_slot(&rig, &run),
                GateStep::SentBack { attempt: 1 }
            );
            assert_eq!(creates(&rig), 0);
            assert_eq!(sends(&rig), vec![sent(1, conflict_message())]);
            let lines: Vec<_> = rig
                .events(&run)
                .into_iter()
                .filter(|event| event.event == "gate_command")
                .collect();
            assert_eq!(lines.len(), 1);
            assert_eq!(lines[0].data["attempt"], 1);
            assert_eq!(lines[0].data["exit_code"], 1);
            assert_eq!(event_count(&rig, &run, "gate_checkout"), 0);
        }

        #[test]
        fn an_unknown_answer_is_gated_as_before_and_waits_for_a_slot() {
            let (rig, run, hub) = built();
            hub.set_mergeable("UNKNOWN");
            assert_eq!(go_without_a_slot(&rig, &run), GateStep::Queued);
            no_writes(&rig);
            assert_eq!(calls(&rig), vec![]);
            assert_eq!(
                rig.steps(&run)
                    .iter()
                    .filter(|s| s.starts_with("gate-"))
                    .count(),
                0
            );
        }

        #[test]
        fn a_mergeable_answer_runs_the_gates_own_checkout_and_merge() {
            let (rig, run, _hub) = built();
            assert_eq!(go(&rig, &run), GateStep::Running);
            assert_eq!(creates(&rig), 1);
            assert_eq!(sends(&rig), vec![]);
        }

        #[test]
        fn the_conflict_counts_toward_gate_retries_like_a_checkout_conflict() {
            let (mut rig, run, hub) = built();
            rig.config.limits.gate_retries = 1;
            hub.set_mergeable("CONFLICTING");
            assert_eq!(go(&rig, &run), GateStep::SentBack { attempt: 1 });
            reported_again(&mut rig, &run, &hub, 2);
            let step = go(&rig, &run);
            assert_stuck(
                &rig,
                &run,
                &step,
                "gate-retries-used-up",
                "the gate failed after 2 attempts: the branch conflicts with the base branch main",
            );
            assert_eq!(sends(&rig).len(), 1);
        }

        #[test]
        fn a_conflict_on_the_last_attempt_is_stuck_with_a_fixed_public_line_and_the_paths_in_the_ledger()
         {
            let (mut rig, run, hub) = built();
            rig.config.limits.gate_retries = 0;
            hub.set_mergeable("CONFLICTING");
            let step = go(&rig, &run);
            assert_stuck(
                &rig,
                &run,
                &step,
                "gate-retries-used-up",
                "the gate failed after 1 attempt: the branch conflicts with the base branch main",
            );
            let GateStep::Stuck { reason } = step else {
                unreachable!()
            };
            assert_eq!(
                reason,
                format!(
                    "the gate failed after 1 attempt\n\n{}",
                    messages::prepare_output_tail(&messages::merge_conflict_text("main", &[]))
                )
            );
        }
    }

    // ----- the runner's `gate: required CI checks` -----

    mod gate_required_ci_checks {
        use super::*;

        const GREEN: &[(&str, &str, Option<&str>)] = &[("ci", "completed", Some("success"))];
        const RED: &[(&str, &str, Option<&str>)] = &[("ci", "completed", Some("failure"))];
        const BUSY: &[(&str, &str, Option<&str>)] = &[("ci", "in_progress", None)];

        /// The gate passes with the project naming the check `ci`, which GitHub shows as `runs`.
        fn gated_with(
            rig: &mut Rig,
            run: &RunId,
            hub: &Hub,
            runs: &[(&str, &str, Option<&str>)],
        ) -> GateStep {
            rig.config.required_checks = vec!["ci".into()];
            hub.set_checks(HEAD, runs);
            gated(rig, run, 0, PASS_TAIL)
        }

        /// The sleep-counting clock moves on by `minutes`, and one pass runs.
        fn go_after(rig: &Rig, run: &RunId, minutes: u64) -> Result<GateStep, RunError> {
            rig.clock.advance(Duration::from_secs(minutes * 60));
            try_go(rig, run)
        }

        fn check_reads(hub: &Hub) -> usize {
            hub.reads()
                .iter()
                .filter(|read| read.contains("check-runs"))
                .count()
        }

        fn red(name: &str) -> FailedCheck {
            FailedCheck {
                name: name.into(),
                conclusion: "failure".into(),
                url: None,
            }
        }

        #[test]
        fn a_pass_with_required_checks_that_have_all_passed_marks_the_pr_ready() {
            let (mut rig, run, hub) = built();
            assert_eq!(
                gated_with(&mut rig, &run, &hub, GREEN),
                GateStep::Passed { pr: 12 }
            );
            let read: Vec<_> = rig
                .events(&run)
                .into_iter()
                .filter(|event| event.event == "checks_read")
                .collect();
            assert_eq!(read.len(), 1);
            assert_eq!(read[0].data["sha"], HEAD);
            assert_eq!(
                read[0].data["checks"],
                json!([{"name": "ci", "state": "passed"}])
            );
            assert_eq!(rig.sim.labels(), ["factory:pr-open"]);
            assert!(!hub.lock().pulls[&12].draft);
        }

        #[test]
        fn a_project_with_no_required_checks_never_reads_them() {
            let (mut rig, run, hub) = built();
            assert_eq!(
                gated(&mut rig, &run, 0, PASS_TAIL),
                GateStep::Passed { pr: 12 }
            );
            assert_eq!(check_reads(&hub), 0);
            assert_eq!(event_count(&rig, &run, "checks_read"), 0);
        }

        #[test]
        fn a_pass_waits_for_checks_that_are_pending_or_missing_and_reads_them_again_on_the_same_commit()
         {
            for (pending, label) in [(BUSY, "pending"), (&[][..], "missing")] {
                let (mut rig, run, hub) = built();
                assert_eq!(
                    gated_with(&mut rig, &run, &hub, pending),
                    GateStep::ChecksPending,
                    "{label}"
                );
                assert!(has_step(&rig, &run, "gate-passed "));
                assert_eq!(event_count(&rig, &run, "checks_read"), 0, "{label}");
                no_writes(&rig);
                assert_eq!(go(&rig, &run), GateStep::ChecksPending, "{label}");
                assert_eq!(check_reads(&hub), 2, "{label}");
                // Not placed again, not asked of the worker again.
                assert_eq!(creates(&rig), 1);
                hub.set_checks(HEAD, GREEN);
                assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 }, "{label}");
                assert_eq!(creates(&rig), 1);
            }
        }

        #[test]
        fn a_failed_check_wakes_the_build_workspace_before_it_is_sent() {
            let (mut rig, run, hub) = built();
            assert_eq!(
                gated_with(&mut rig, &run, &hub, RED),
                GateStep::SentBack { attempt: 1 }
            );
            woke_before_the_send(&rig);
        }

        #[test]
        fn a_failed_check_goes_back_into_the_session_and_uses_a_gate_attempt() {
            let (mut rig, run, hub) = built();
            assert_eq!(
                gated_with(&mut rig, &run, &hub, RED),
                GateStep::SentBack { attempt: 1 }
            );
            assert_eq!(
                sends(&rig),
                vec![sent(
                    1,
                    messages::check_failed(HEAD, &[red("ci")], BODY_PATH)
                )]
            );
            assert_eq!(event_count(&rig, &run, "gate_sent_back"), 1);
            assert_eq!(event_count(&rig, &run, "checks_read"), 1);
            assert_eq!(rig.sim.labels(), ["factory:building"]);
            reported_again(&mut rig, &run, &hub, 2);
            rig.worker.on_create(Ok(gate_ws(2)));
            assert_eq!(go(&rig, &run), GateStep::Running);
            assert!(has_step(&rig, &run, &format!("{GATE_REQUESTED}2 2 {HEAD}")));
        }

        #[test]
        fn a_failed_check_is_reported_even_while_another_is_pending_and_every_failed_one_is_named()
        {
            let (mut rig, run, hub) = built();
            rig.config.required_checks = vec!["lint".into(), "test".into(), "docs".into()];
            hub.set_checks(
                HEAD,
                &[
                    ("lint", "completed", Some("failure")),
                    ("test", "completed", Some("failure")),
                    ("docs", "in_progress", None),
                ],
            );
            assert_eq!(
                gated(&mut rig, &run, 0, PASS_TAIL),
                GateStep::SentBack { attempt: 1 }
            );
            assert_eq!(
                sends(&rig),
                vec![sent(
                    1,
                    messages::check_failed(HEAD, &[red("lint"), red("test")], BODY_PATH)
                )]
            );
        }

        #[test]
        fn a_failed_check_on_the_last_attempt_is_stuck_like_a_failed_command() {
            let (mut rig, run, hub) = built();
            rig.config.limits.gate_retries = 0;
            let step = gated_with(&mut rig, &run, &hub, RED);
            assert_stuck(
                &rig,
                &run,
                &step,
                "checks-retries-used-up",
                "the required checks failed after 1 attempt",
            );
            let text = messages::checks_failure_text(HEAD, &[red("ci")]);
            assert_eq!(
                step,
                GateStep::Stuck {
                    reason: format!(
                        "the required checks failed after 1 attempt\n\n{}",
                        messages::prepare_output_tail(&text)
                    )
                }
            );
            assert_eq!(sends(&rig), vec![]);
        }

        #[test]
        fn a_failed_check_after_the_retries_are_used_says_how_many_attempts_there_were() {
            let (mut rig, run, hub) = built();
            rig.config.limits.gate_retries = 1;
            assert_eq!(
                gated_with(&mut rig, &run, &hub, RED),
                GateStep::SentBack { attempt: 1 }
            );
            reported_again(&mut rig, &run, &hub, 2);
            place(&mut rig, &run, 2, 2);
            let step = ends(&mut rig, &run, 2, 2, 0, PASS_TAIL);
            assert_stuck(
                &rig,
                &run,
                &step,
                "checks-retries-used-up",
                "the required checks failed after 2 attempts",
            );
        }

        #[test]
        fn checks_that_have_not_finished_when_the_limit_ends_send_the_issue_to_stuck() {
            let (mut rig, run, hub) = built();
            assert_eq!(
                gated_with(&mut rig, &run, &hub, BUSY),
                GateStep::ChecksPending
            );
            assert_eq!(go_after(&rig, &run, 29).unwrap(), GateStep::ChecksPending);
            let step = go_after(&rig, &run, 2).unwrap();
            assert_stuck(
                &rig,
                &run,
                &step,
                "checks-timed-out",
                "the required checks did not finish within 30 minutes",
            );
            let read = rig
                .events(&run)
                .into_iter()
                .find(|event| event.event == "checks_read")
                .expect("the read that timed out");
            assert_eq!(read.cause.unwrap()["reason"], "checks-timed-out");
        }

        #[test]
        fn a_check_that_never_appeared_when_the_limit_ends_has_its_own_fixed_reason() {
            let (mut rig, run, hub) = built();
            assert_eq!(
                gated_with(&mut rig, &run, &hub, &[]),
                GateStep::ChecksPending
            );
            let step = go_after(&rig, &run, 31).unwrap();
            assert_stuck(
                &rig,
                &run,
                &step,
                "checks-timed-out",
                "a required check was not reported within 30 minutes",
            );
        }

        #[test]
        fn a_timed_out_read_recorded_before_a_crash_is_the_decision_even_if_the_checks_pass_later()
        {
            let (mut rig, run, hub) = built();
            gated_with(&mut rig, &run, &hub, BUSY);
            // The lease is lost after the read is recorded and before the stuck path ends the
            // session: the pass's one check is the wait's own, read before the checks.
            rig.guard = Box::new(HeldFor::new(1));
            assert!(matches!(
                go_after(&rig, &run, 31),
                Err(RunError::LeaseLost(_))
            ));
            assert_eq!(event_count(&rig, &run, "checks_read"), 1);
            rig.guard = Box::new(AlwaysHeld(1));
            hub.set_checks(HEAD, GREEN);
            let step = try_go(&rig, &run).unwrap();
            assert_stuck(
                &rig,
                &run,
                &step,
                "checks-timed-out",
                "the required checks did not finish within 30 minutes",
            );
            assert!(!rig.sim.writes().iter().any(|w| w.contains("pr ready 12")));
        }

        #[test]
        fn checks_that_have_passed_when_the_limit_ends_still_count() {
            let (mut rig, run, hub) = built();
            gated_with(&mut rig, &run, &hub, BUSY);
            hub.set_checks(HEAD, GREEN);
            assert_eq!(
                go_after(&rig, &run, 31).unwrap(),
                GateStep::Passed { pr: 12 }
            );
        }

        #[test]
        fn a_head_that_moved_while_the_checks_ran_is_gated_again_and_nothing_is_marked_ready() {
            let (mut rig, run, hub) = built();
            assert_eq!(
                gated_with(&mut rig, &run, &hub, BUSY),
                GateStep::ChecksPending
            );
            hub.set_head(MOVED);
            assert_eq!(go(&rig, &run), GateStep::NotReady);
            assert!(!rig.sim.writes().iter().any(|w| w.contains("pr ready 12")));
            assert_eq!(rig.sim.labels(), ["factory:building"]);
            // The next pass gates the new head as the next attempt.
            rig.worker.on_create(Ok(gate_ws(2)));
            assert_eq!(go(&rig, &run), GateStep::Running);
            assert!(has_step(
                &rig,
                &run,
                &format!("{GATE_REQUESTED}2 2 {MOVED}")
            ));
        }

        #[test]
        fn a_pull_request_that_is_no_longer_open_is_not_marked_ready_and_the_checks_are_not_read() {
            let (mut rig, run, hub) = built();
            gated_with(&mut rig, &run, &hub, BUSY);
            let reads = check_reads(&hub);
            hub.set_state("MERGED");
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
            assert_eq!(check_reads(&hub), reads);
            assert!(
                !rig.sim
                    .writes()
                    .iter()
                    .any(|w| w.contains("pr ready 12") || w.contains("pr comment"))
            );
            assert_eq!(rig.sim.labels(), ["factory:pr-open"]);
        }

        #[test]
        fn a_pass_whose_checks_were_all_read_as_passed_does_not_read_them_again_after_a_crash() {
            let mut cut_after_the_read = 0;
            for checks in 1..=14 {
                let (mut rig, run, hub) = built();
                rig.config.required_checks = vec!["ci".into()];
                hub.set_checks(HEAD, GREEN);
                place(&mut rig, &run, 1, 1);
                show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 0)]);
                rig.worker.on_tail(Ok(PASS_TAIL.into()));
                rig.guard = Box::new(HeldFor::new(checks));
                let first = try_go(&rig, &run);
                let reads = check_reads(&hub);
                if first.is_err() && event_count(&rig, &run, "checks_read") == 1 {
                    cut_after_the_read += 1;
                    rig.guard = Box::new(AlwaysHeld(1));
                    assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 }, "{checks}");
                    assert_eq!(check_reads(&hub), reads, "{checks}");
                }
            }
            assert!(
                cut_after_the_read > 0,
                "a cut fell after the read was recorded"
            );
        }

        #[test]
        fn a_failure_whose_send_reached_the_session_is_recorded_even_though_the_head_moved() {
            let (mut rig, run, hub) = built();
            rig.config.required_checks = vec!["ci".into()];
            hub.set_checks(HEAD, RED);
            place(&mut rig, &run, 1, 1);
            rig.worker.on_send(Err(SendRefused::Failed {
                reason: "the answer was lost".into(),
                prompts: Some(1),
            }));
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 0)]);
            rig.worker.on_tail(Ok(PASS_TAIL.into()));
            assert!(matches!(try_go(&rig, &run), Err(RunError::Send(_))));
            hub.set_head(MOVED);
            rig.worker.on_send(Err(SendRefused::CountRose { now: 2 }));
            show(&mut rig, &run, SessionState::Idle, 2, &[]);
            assert_eq!(go(&rig, &run), GateStep::SentBack { attempt: 1 });
            assert_eq!(event_count(&rig, &run, "gate_sent_back"), 1);
        }

        #[test]
        fn a_failed_check_waits_while_the_session_is_still_working_and_sends_nothing() {
            let (mut rig, run, hub) = built();
            gated_with(&mut rig, &run, &hub, BUSY);
            hub.set_checks(HEAD, RED);
            show(&mut rig, &run, SessionState::Working, 2, &[]);
            assert_eq!(go(&rig, &run), GateStep::NotReady);
            assert_eq!(sends(&rig), vec![]);
            assert_eq!(event_count(&rig, &run, "gate_sent_back"), 0);
        }

        #[test]
        fn a_failed_check_waits_when_the_session_has_no_run_ordinal() {
            let (mut rig, run, hub) = built();
            gated_with(&mut rig, &run, &hub, BUSY);
            hub.set_checks(HEAD, RED);
            show(&mut rig, &run, SessionState::Idle, 0, &[]);
            assert_eq!(go(&rig, &run), GateStep::NotReady);
            assert_eq!(sends(&rig), vec![]);
        }

        #[test]
        fn a_failure_that_may_not_have_reached_the_session_is_sent_again_under_the_same_command_id()
        {
            let (mut rig, run, hub) = built();
            rig.config.required_checks = vec!["ci".into()];
            hub.set_checks(HEAD, RED);
            place(&mut rig, &run, 1, 1);
            rig.worker.on_send(Err(SendRefused::Failed {
                reason: "the terminal did not answer".into(),
                prompts: Some(1),
            }));
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 0)]);
            rig.worker.on_tail(Ok(PASS_TAIL.into()));
            assert!(matches!(try_go(&rig, &run), Err(RunError::Send(_))));
            assert_eq!(go(&rig, &run), GateStep::SentBack { attempt: 1 });
            let message = messages::check_failed(HEAD, &[red("ci")], BODY_PATH);
            assert_eq!(
                sends(&rig),
                vec![sent(1, message.clone()), sent(1, message)]
            );
            assert_eq!(event_count(&rig, &run, "gate_sent_back"), 1);
        }

        #[test]
        fn a_session_that_is_gone_when_a_failure_may_have_been_sent_is_stuck() {
            let (mut rig, run, hub) = built();
            rig.config.required_checks = vec!["ci".into()];
            hub.set_checks(HEAD, RED);
            place(&mut rig, &run, 1, 1);
            rig.worker.on_send(Err(SendRefused::Failed {
                reason: "the terminal did not answer".into(),
                prompts: Some(1),
            }));
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 0)]);
            rig.worker.on_tail(Ok(PASS_TAIL.into()));
            assert!(matches!(try_go(&rig, &run), Err(RunError::Send(_))));
            rig.view = gate_machine(FleetView::builder()).build();
            let step = go(&rig, &run);
            assert_stuck(&rig, &run, &step, "session-gone", SESSION_GONE);
        }

        #[test]
        fn a_session_that_is_gone_when_a_check_has_failed_is_stuck() {
            let (mut rig, run, hub) = built();
            gated_with(&mut rig, &run, &hub, BUSY);
            hub.set_checks(HEAD, RED);
            rig.view = gate_machine(FleetView::builder()).build();
            let step = go(&rig, &run);
            assert_stuck(&rig, &run, &step, "session-gone", SESSION_GONE);
        }

        #[test]
        fn a_failing_read_of_the_checks_is_an_error_that_decides_nothing() {
            let (mut rig, run, hub) = built();
            gated_with(&mut rig, &run, &hub, BUSY);
            hub.set_checks(HEAD, GREEN);
            hub.fail("check-runs", 1);
            let before = rig.events(&run).len();
            assert!(matches!(try_go(&rig, &run), Err(RunError::Tracker(_))));
            assert_eq!(rig.events(&run).len(), before);
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
        }

        #[test]
        fn a_failed_check_read_before_a_lost_send_is_the_decision_even_if_the_check_passes_on_a_rerun()
         {
            let (mut rig, run, hub) = built();
            rig.config.required_checks = vec!["ci".into()];
            hub.set_checks(HEAD, RED);
            place(&mut rig, &run, 1, 1);
            rig.worker.on_send(Err(SendRefused::Failed {
                reason: "the terminal did not answer".into(),
                prompts: Some(1),
            }));
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 0)]);
            rig.worker.on_tail(Ok(PASS_TAIL.into()));
            assert!(matches!(try_go(&rig, &run), Err(RunError::Send(_))));
            hub.set_checks(HEAD, GREEN);
            assert_eq!(go(&rig, &run), GateStep::SentBack { attempt: 1 });
            assert!(
                !rig.sim
                    .writes()
                    .iter()
                    .any(|w| w.contains("pr ready 12") && !w.contains("--undo"))
            );
        }

        #[test]
        fn a_failed_check_read_before_a_lost_send_is_not_timed_out_while_the_session_waits() {
            let (mut rig, run, hub) = built();
            rig.config.required_checks = vec!["ci".into()];
            hub.set_checks(HEAD, RED);
            place(&mut rig, &run, 1, 1);
            rig.worker.on_send(Err(SendRefused::Failed {
                reason: "the terminal did not answer".into(),
                prompts: Some(1),
            }));
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 0)]);
            rig.worker.on_tail(Ok(PASS_TAIL.into()));
            assert!(matches!(try_go(&rig, &run), Err(RunError::Send(_))));
            show(&mut rig, &run, SessionState::Working, 2, &[]);
            assert_eq!(go_after(&rig, &run, 31).unwrap(), GateStep::NotReady);
            assert!(!rig.finished(&run));
        }

        #[test]
        fn a_failed_check_puts_a_pr_someone_marked_ready_while_the_checks_ran_back_to_draft() {
            let (mut rig, run, hub) = built();
            gated_with(&mut rig, &run, &hub, BUSY);
            hub.set_draft(false);
            hub.set_checks(HEAD, RED);
            assert_eq!(go(&rig, &run), GateStep::SentBack { attempt: 1 });
            assert!(hub.lock().pulls[&12].draft);
            assert_eq!(event_count(&rig, &run, "pr_set_draft"), 1);
        }

        #[test]
        fn a_failed_check_whose_pr_cannot_be_put_back_to_draft_sends_the_issue_to_stuck() {
            let (mut rig, run, hub) = built();
            gated_with(&mut rig, &run, &hub, BUSY);
            hub.set_draft(false);
            hub.set_checks(HEAD, RED);
            rig.sim.fail("--undo", 2);
            let step = go(&rig, &run);
            assert_stuck(
                &rig,
                &run,
                &step,
                "pr-not-bound",
                "the session reported PR #12 but it is not a draft",
            );
            assert_eq!(sends(&rig), vec![]);
        }
    }

    // ----- the runner's `gate: a closed issue` -----

    mod gate_a_closed_issue {
        use super::*;

        #[test]
        fn ends_the_run_writes_nothing_on_github_runs_no_command_and_archives_once() {
            let (rig, run, _hub) = built();
            rig.sim.set_closed(true);
            assert_eq!(go(&rig, &run), GateStep::Abandoned);
            no_writes(&rig);
            assert_eq!(calls(&rig), vec![end_of_build()]);
            assert!(rig.finished(&run));
            let last = rig.events(&run).pop().unwrap();
            assert_eq!(last.data["outcome"], "stuck");
            assert_eq!(last.cause.unwrap()["reason"], "issue-closed");
        }

        #[test]
        fn whose_archive_fails_is_an_error_with_nothing_written_and_the_next_call_ends_it() {
            let (rig, run, _hub) = built();
            rig.sim.set_closed(true);
            rig.worker
                .on_end_sessions(Err(WorkerError::Unreachable("the machine is gone".into())));
            assert!(matches!(try_go(&rig, &run), Err(RunError::Worker(_))));
            assert!(!rig.finished(&run));
            no_writes(&rig);
            assert_eq!(go(&rig, &run), GateStep::Abandoned);
            assert!(rig.finished(&run));
        }

        #[test]
        fn that_cannot_be_read_is_an_error_and_never_an_abandon() {
            let (rig, run, _hub) = built();
            rig.sim.set_closed(true);
            rig.sim.fail("issue view", 1);
            assert!(matches!(try_go(&rig, &run), Err(RunError::Tracker(_))));
            assert!(!rig.finished(&run));
            assert_eq!(calls(&rig), vec![]);
        }

        #[test]
        fn is_not_asked_about_for_a_run_the_gate_would_not_look_at() {
            let (rig, run, _hub) = built();
            rig.sim.set_closed(true);
            rig.moved(&run, State::Building, State::PrOpen);
            rig.step(&run, BUILD_SESSION_ENDED);
            rig.step(&run, BUILD_ARCHIVED);
            let before = rig.sim.calls().len();
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
            assert_eq!(rig.sim.calls().len(), before);
        }

        #[test]
        fn closed_by_a_merge_before_the_gate_passed_is_the_runs_own_finish_not_an_abandon() {
            let (rig, run, hub) = built();
            rig.sim.set_closed(true);
            hub.set_state("MERGED");
            assert_eq!(go(&rig, &run), GateStep::Ended);
            let last = rig.events(&run).pop().unwrap();
            assert_eq!(last.event, "finished");
            assert_eq!(last.data["pr_end"], "merged");
            assert_ne!(last.data["outcome"], "stuck");
            no_writes(&rig);
        }

        #[test]
        fn closed_by_the_merge_of_a_pull_request_whose_pass_is_recorded_is_finished_without_marking_it_ready()
         {
            let (mut rig, run, hub) = built();
            rig.config.required_checks = vec!["ci".into()];
            hub.set_checks(HEAD, &[("ci", "in_progress", None)]);
            assert_eq!(gated(&mut rig, &run, 0, PASS_TAIL), GateStep::ChecksPending);
            rig.sim.set_closed(true);
            hub.set_state("MERGED");
            // The pass is finished as it stands: the label, and no `ready` or results on a
            // pull request that is merged. Ending the run is the feedback step's.
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
            assert!(
                !rig.sim
                    .writes()
                    .iter()
                    .any(|w| w.contains("pr ready 12") || w.contains("pr comment"))
            );
            assert_eq!(rig.sim.labels(), ["factory:pr-open"]);
            assert!(!rig.finished(&run));
        }

        #[test]
        fn while_its_pull_request_is_closed_unmerged_is_abandoned() {
            let (rig, run, hub) = built();
            rig.sim.set_closed(true);
            hub.set_state("CLOSED");
            assert_eq!(go(&rig, &run), GateStep::Abandoned);
            assert_eq!(creates(&rig), 0);
            no_writes(&rig);
        }
    }

    // ----- the runner's `gate: a run stopped by hand` -----

    mod gate_a_run_stopped_by_hand {
        use super::*;

        fn stop(rig: &Rig, run: &RunId) {
            rig.write(run, Event::StopRequested { by: "tom".into() });
        }

        #[test]
        fn is_finished_as_stuck_by_hand_and_runs_no_command() {
            let (rig, run, _hub) = built();
            stop(&rig, &run);
            let step = go(&rig, &run);
            assert_stuck(&rig, &run, &step, "stop-requested", STOPPED_BY_HAND);
            assert_eq!(creates(&rig), 0);
        }

        #[test]
        fn is_ended_as_a_closed_issue_first_when_the_issue_was_closed_too() {
            let (rig, run, _hub) = built();
            stop(&rig, &run);
            rig.sim.set_closed(true);
            assert_eq!(go(&rig, &run), GateStep::Abandoned);
            no_writes(&rig);
        }

        #[test]
        fn is_not_stopped_by_the_gate_once_its_pull_request_is_open_and_waiting() {
            let (rig, run, _hub) = built();
            rig.moved(&run, State::Building, State::PrOpen);
            rig.step(&run, BUILD_SESSION_ENDED);
            rig.step(&run, BUILD_ARCHIVED);
            stop(&rig, &run);
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
            assert!(!rig.finished(&run));
            assert_eq!(calls(&rig), vec![]);
        }
    }

    // ----- what the pass adds to the runner's gate -----

    mod foundrys_own {
        use super::*;

        fn with_note(message: messages::Message) -> messages::Message {
            messages::Message::new(
                message.line,
                format!("{}\n\n{CONFIG_DIFFERS}", message.body.unwrap()),
            )
        }

        #[test]
        fn reaching_pr_open_leaves_no_live_session_and_no_workspace_of_the_run() {
            let (mut rig, run, hub) = built();
            gated(&mut rig, &run, 101, FAIL_TAIL);
            reported_again(&mut rig, &run, &hub, 2);
            place(&mut rig, &run, 2, 2);
            assert_eq!(
                ends(&mut rig, &run, 2, 2, 0, PASS_TAIL),
                GateStep::Passed { pr: 12 }
            );
            // The build and both gates' workspaces are archived, each once, and the build's
            // sessions are ended before its workspace is.
            let mut gone = archived(&rig);
            gone.sort();
            let mut all = vec![workspace(), gate_ws(1), gate_ws(2)];
            all.sort();
            assert_eq!(gone, all);
            let order = calls(&rig);
            let ended = order.iter().position(|c| *c == end_of_build()).unwrap();
            let build = order
                .iter()
                .position(|c| *c == archive_of(workspace()))
                .unwrap();
            assert!(ended < build);
            assert_eq!(rig.sim.labels(), ["factory:pr-open"]);
            assert!(!gate_running(&rig.events(&run)));
            // Nothing of the run is left to ask for on a later pass.
            let before = rig.worker.calls().len();
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
            assert_eq!(rig.worker.calls().len(), before);
        }

        #[test]
        fn a_gate_that_could_not_run_backs_off_1_2_4_8_passes_then_stuck_and_spends_no_retry() {
            let (mut rig, run, _hub) = built();
            rig.config.limits.gate_retries = 0;
            for _ in 0..6 {
                rig.worker
                    .on_create(Err(WorkerError::Failed("disk full".into())));
            }
            let mut tries = vec![];
            let mut results = vec![];
            for pass in 1..=30 {
                let before = creates(&rig);
                let step = go(&rig, &run);
                if creates(&rig) > before {
                    tries.push(pass);
                }
                let stuck = matches!(step, GateStep::Stuck { .. });
                results.push(step);
                if stuck {
                    break;
                }
            }
            assert_eq!(tries, [1, 3, 6, 11, 20]);
            let mut expected: Vec<GateStep> = vec![];
            for wait in [1u32, 2, 4, 8] {
                for left in (0..=wait).rev() {
                    expected.push(GateStep::BackingOff { passes_left: left });
                }
            }
            let last = results.pop().unwrap();
            assert_eq!(results, expected);
            let GateStep::Stuck { reason } = last else {
                panic!("expected a stuck run");
            };
            assert!(reason.contains("disk full"), "{reason}");
            assert_eq!(step_count(&rig, &run, GATE_COULD_NOT_RUN), 5);
            assert_eq!(step_count(&rig, &run, GATE_WAITING), 15);
            assert_eq!(step_count(&rig, &run, GATE_LEFT), 5);
            assert_eq!(rig.sim.labels(), ["factory:stuck"]);
            assert!(
                rig.sim
                    .comments()
                    .iter()
                    .any(|comment| comment.contains("the gate could not run"))
            );
            // The worker's words are in the ledger's detail, never in the public comment.
            assert!(
                rig.sim
                    .comments()
                    .iter()
                    .all(|comment| !comment.contains("disk full"))
            );
            assert!(
                rig.steps(&run)
                    .iter()
                    .any(|step| step.starts_with("stuck-detail ") && step.contains("disk full"))
            );
            let finish = rig.events(&run).pop().unwrap();
            assert_eq!(finish.cause.unwrap()["reason"], "gate-could-not-run");
            // No gate ran: no checkout, no command, nothing sent back, no retry spent.
            assert_eq!(event_count(&rig, &run, "gate_checkout"), 0);
            assert_eq!(event_count(&rig, &run, "gate_command"), 0);
            assert_eq!(sends(&rig), vec![]);
            assert!(!gate_running(&rig.events(&run)));
        }

        #[test]
        fn a_gate_that_could_not_run_is_not_counted_against_gate_retries() {
            let (mut rig, run, _hub) = built();
            rig.config.limits.gate_retries = 0;
            rig.worker
                .on_create(Err(WorkerError::Unreachable("no route".into())));
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 1 });
            rig.worker.on_create(Ok(gate_ws(2)));
            // The first gate is looked for until it is left; then the second is placed.
            let mut passes = 0;
            while go(&rig, &run) != GateStep::Running {
                passes += 1;
                assert!(passes < 20);
            }
            assert_eq!(step_count(&rig, &run, GATE_LEFT), 1);
            // Still the first attempt, in the second workspace.
            assert!(has_step(&rig, &run, &format!("{GATE_REQUESTED}1 2 {HEAD}")));
            show(&mut rig, &run, SessionState::Idle, 1, &[running(2)]);
            let step = ends(&mut rig, &run, 1, 2, 101, FAIL_TAIL);
            assert_stuck(
                &rig,
                &run,
                &step,
                "gate-retries-used-up",
                "the gate failed after 1 attempt: `cargo test` failed (exit code 101)",
            );
        }

        #[test]
        fn a_gate_with_no_machine_that_qualifies_backs_off() {
            let (mut rig, run, _hub) = built();
            rig.view = FleetView::builder()
                .online(builder())
                .session(session_record(SessionState::Idle))
                .turn(SessionTurn {
                    session: session(),
                    workspace: workspace(),
                    project: project_id(),
                    turn: 1,
                    prompts: 1,
                    question: false,
                })
                .build();
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 1 });
            assert_eq!(creates(&rig), 0);
            assert!(has_step(&rig, &run, GATE_COULD_NOT_RUN));
            assert!(
                rig.steps(&run)
                    .iter()
                    .any(|step| step.contains("no machine can take the gate"))
            );
        }

        #[test]
        fn a_gate_whose_machine_is_unreachable_for_five_minutes_could_not_run_and_is_tried_again() {
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            assert!(gate_running(&rig.events(&run)));
            show_gone(
                &mut rig,
                &run,
                1,
                &[running(1)],
                Duration::from_secs(4 * 60),
            );
            assert_eq!(go(&rig, &run), GateStep::Running);
            show_gone(
                &mut rig,
                &run,
                1,
                &[running(1)],
                Duration::from_secs(6 * 60),
            );
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 1 });
            assert!(
                rig.steps(&run).iter().any(
                    |step| step.starts_with(GATE_COULD_NOT_RUN) && step.contains("unreachable")
                )
            );
            // The slot is free again.
            assert!(!gate_running(&rig.events(&run)));
            // Nothing is asked of the machine that is gone, and nothing is sent to the session.
            assert!(!calls(&rig).iter().any(|call| matches!(
                call,
                Call::Tail { .. } | Call::Archive { .. } | Call::Send { .. }
            )));
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 0 });
            show(&mut rig, &run, SessionState::Idle, 1, &[]);
            rig.worker.on_create(Ok(gate_ws(2)));
            assert_eq!(go(&rig, &run), GateStep::Running);
            assert!(has_step(&rig, &run, &format!("{GATE_REQUESTED}1 2 {HEAD}")));
        }

        #[test]
        fn a_gate_on_the_build_machine_cut_off_could_not_run_and_is_tried_again_while_the_session_is_away()
         {
            // The fleet's one-machine case in the rig: the session is shown unreachable while
            // its gate's machine is cut off mid-gate.
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            let cut_off = |rig: &mut Rig, minutes: u64| {
                show_gone_with(
                    rig,
                    &run,
                    SessionState::Unreachable,
                    1,
                    &[running(1)],
                    Duration::from_secs(minutes * 60),
                );
            };
            cut_off(&mut rig, 4);
            assert_eq!(go(&rig, &run), GateStep::Running);
            cut_off(&mut rig, 6);
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 1 });
            assert!(
                rig.steps(&run).iter().any(
                    |step| step.starts_with(GATE_COULD_NOT_RUN) && step.contains("unreachable")
                )
            );
            assert!(!gate_running(&rig.events(&run)));
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 0 });
            // The session is still away: the gate is placed again all the same.
            show(&mut rig, &run, SessionState::Unreachable, 1, &[]);
            rig.worker.on_create(Ok(gate_ws(2)));
            assert_eq!(go(&rig, &run), GateStep::Running);
            assert!(has_step(&rig, &run, &format!("{GATE_REQUESTED}1 2 {HEAD}")));
            assert!(
                !calls(&rig)
                    .iter()
                    .any(|call| matches!(call, Call::Send { .. })),
                "nothing is sent to the session"
            );
        }

        #[test]
        fn a_gate_whose_terminal_exited_with_no_exit_code_could_not_run_and_is_not_a_failure() {
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            show(
                &mut rig,
                &run,
                SessionState::Idle,
                1,
                &[G {
                    k: 1,
                    state: TerminalState::Exited,
                    code: None,
                }],
            );
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 1 });
            assert_eq!(sends(&rig), vec![]);
            assert_eq!(event_count(&rig, &run, "gate_sent_back"), 0);
            assert!(!gate_running(&rig.events(&run)));
            // Its machine is not gone: the workspace is archived by force before another gate
            // is placed.
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 0 });
            assert_eq!(archived(&rig), vec![gate_ws(1)]);
            assert!(has_step(&rig, &run, &format!("{GATE_ARCHIVING}1")));
            assert!(has_step(&rig, &run, &format!("{GATE_ARCHIVED}1")));
            show(&mut rig, &run, SessionState::Idle, 1, &[]);
            rig.worker.on_create(Ok(gate_ws(2)));
            assert_eq!(go(&rig, &run), GateStep::Running);
            assert_eq!(archived(&rig), vec![gate_ws(1)]);
        }

        #[test]
        fn a_gate_that_is_running_is_not_placed_again_and_a_create_whose_record_comes_later_is_not_asked_twice()
         {
            let (mut rig, run, _hub) = built();
            rig.worker.on_create(Ok(gate_ws(1)));
            assert_eq!(go(&rig, &run), GateStep::Running);
            // The workspace's record has not reached the view yet.
            assert_eq!(go(&rig, &run), GateStep::Running);
            show(&mut rig, &run, SessionState::Idle, 1, &[running(1)]);
            assert_eq!(go(&rig, &run), GateStep::Running);
            assert_eq!(creates(&rig), 1);
            assert_eq!(step_count(&rig, &run, GATE_REQUESTED), 1);
        }

        #[test]
        fn a_workspace_the_view_already_shows_under_the_gates_name_is_not_asked_for_again() {
            let (mut rig, run, _hub) = built();
            rig.guard = Box::new(Lost);
            assert!(matches!(try_go(&rig, &run), Err(RunError::LeaseLost(_))));
            rig.guard = Box::new(AlwaysHeld(1));
            // The create was in flight when the lease lapsed, and its workspace is there now.
            show(&mut rig, &run, SessionState::Idle, 1, &[running(1)]);
            assert_eq!(go(&rig, &run), GateStep::Running);
            assert_eq!(creates(&rig), 0);
            assert!(has_step(&rig, &run, GATE_CREATED));
        }

        #[test]
        fn every_step_is_recorded_before_it_is_asked_for() {
            let (mut rig, run, _hub) = built();
            rig.guard = Box::new(Lost);
            assert!(matches!(try_go(&rig, &run), Err(RunError::LeaseLost(_))));
            // The commit gated is on record, and the worker was not asked.
            assert_eq!(
                rig.steps(&run)
                    .iter()
                    .filter(|step| step.starts_with(GATE_REQUESTED))
                    .cloned()
                    .collect::<Vec<_>>(),
                [format!(
                    "{GATE_REQUESTED}1 1 {HEAD} {BASE_TIP} {}",
                    machine()
                )]
            );
            assert_eq!(calls(&rig), vec![]);
            no_writes(&rig);
            // The next pass carries on from the record: one request, one create.
            rig.guard = Box::new(AlwaysHeld(1));
            rig.worker.on_create(Ok(gate_ws(1)));
            assert_eq!(go(&rig, &run), GateStep::Running);
            assert_eq!(step_count(&rig, &run, GATE_REQUESTED), 1);
            assert_eq!(creates(&rig), 1);
            // The verdict is recorded before the workspace is archived.
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 101)]);
            rig.worker.on_tail(Ok(FAIL_TAIL.into()));
            rig.worker
                .on_archive(Err(WorkerError::Failed("the worktree is busy".into())));
            assert!(matches!(try_go(&rig, &run), Err(RunError::Worker(_))));
            assert!(has_step(&rig, &run, &format!("{GATE_ENDED}1 101")));
            assert!(has_step(&rig, &run, &format!("{GATE_ARCHIVING}1")));
            assert!(!has_step(&rig, &run, GATE_ARCHIVED));
        }

        #[test]
        fn the_gate_output_tail_kept_in_the_event_is_cut_to_its_last_64_kb() {
            let (mut rig, run, _hub) = built();
            rig.config.limits.gate_retries = 0;
            let noise = "x".repeat(200_000);
            let tail = format!("$ cargo test\n{noise}\nsetup failed: cargo test exited 101\n");
            let step = gated(&mut rig, &run, 101, &tail);
            assert!(matches!(step, GateStep::Stuck { .. }));
            // The verdict is a short step of its own; the output is an event's field, cut to
            // its last 64 KB.
            assert!(has_step(&rig, &run, &format!("{GATE_ENDED}1 101")));
            let kept: Vec<String> = rig
                .events(&run)
                .into_iter()
                .filter(|event| event.event == "gate_cleanup_failed")
                .map(|event| event.data["error"].as_str().unwrap().to_owned())
                .collect();
            assert_eq!(kept.len(), 1);
            assert!(kept[0].len() <= 64 * 1024, "{}", kept[0].len());
            assert!(kept[0].ends_with("setup failed: cargo test exited 101\n"));
            // What goes to a comment is cut much shorter, and holds no output.
            assert!(
                rig.sim
                    .comments()
                    .iter()
                    .all(|comment| comment.len() < 20_000)
            );
        }

        #[test]
        fn a_failure_sent_back_says_when_the_config_at_the_head_differs_from_the_default_branchs() {
            let (mut rig, run, hub) = built();
            hub.set_file(HEAD, "factory:\n  gate:\n    - true\n");
            assert_eq!(
                gated(&mut rig, &run, 101, FAIL_TAIL),
                GateStep::SentBack { attempt: 1 }
            );
            assert_eq!(
                sends(&rig),
                vec![sent(1, with_note(failed_message(FAIL_OUTPUT)))]
            );
            assert!(
                sends(&rig).iter().all(|call| matches!(
                    call,
                    Call::Send { message, .. } if message.body.as_ref().unwrap().contains(CONFIG_DIFFERS)
                ))
            );
        }

        #[test]
        fn a_failure_sent_back_says_so_when_the_head_has_no_config_at_all() {
            let (mut rig, run, hub) = built();
            hub.lock().files.remove(HEAD);
            assert_eq!(
                gated(&mut rig, &run, 101, FAIL_TAIL),
                GateStep::SentBack { attempt: 1 }
            );
            assert_eq!(
                sends(&rig),
                vec![sent(1, with_note(failed_message(FAIL_OUTPUT)))]
            );
        }

        #[test]
        fn a_failed_check_sent_back_says_when_the_config_at_the_head_differs() {
            let (mut rig, run, hub) = built();
            rig.config.required_checks = vec!["ci".into()];
            hub.set_checks(HEAD, &[("ci", "completed", Some("failure"))]);
            hub.set_file(HEAD, "factory:\n  gate:\n    - true\n");
            assert_eq!(
                gated(&mut rig, &run, 0, PASS_TAIL),
                GateStep::SentBack { attempt: 1 }
            );
            let failed = FailedCheck {
                name: "ci".into(),
                conclusion: "failure".into(),
                url: None,
            };
            assert_eq!(
                sends(&rig),
                vec![sent(
                    1,
                    with_note(messages::check_failed(HEAD, &[failed], BODY_PATH))
                )]
            );
        }

        #[test]
        fn a_stuck_comment_says_when_the_config_at_the_head_differs_from_the_default_branchs() {
            let (mut rig, run, hub) = built();
            rig.config.limits.gate_retries = 0;
            hub.set_file(HEAD, "factory:\n  gate:\n    - true\n");
            let step = gated(&mut rig, &run, 101, FAIL_TAIL);
            assert_stuck(
                &rig,
                &run,
                &step,
                "gate-retries-used-up",
                &format!(
                    "the gate failed after 1 attempt: `cargo test` failed (exit code 101). {CONFIG_DIFFERS}"
                ),
            );
        }

        #[test]
        fn a_config_that_is_the_default_branchs_is_not_mentioned() {
            let (mut rig, run, _hub) = built();
            rig.config.limits.gate_retries = 0;
            let step = gated(&mut rig, &run, 101, FAIL_TAIL);
            assert_stuck(
                &rig,
                &run,
                &step,
                "gate-retries-used-up",
                "the gate failed after 1 attempt: `cargo test` failed (exit code 101)",
            );
            assert!(
                rig.sim
                    .comments()
                    .iter()
                    .all(|c| !c.contains(CONFIG_DIFFERS))
            );
        }

        #[test]
        fn a_gate_placed_while_no_gate_slot_is_free_is_not_placed() {
            let (rig, run, _hub) = built();
            let mut none = Slots {
                runs_free: 1,
                gates_free: 0,
            };
            let step = advance(&rig.ctx(), &run, &shared(), &mut none);
            assert_eq!(step.unwrap(), GateStep::Queued);
            assert_eq!(calls(&rig), vec![]);
            no_writes(&rig);
            assert_eq!(step_count(&rig, &run, "gate-"), 0);
            assert_eq!(none.gates_free, 0);
        }

        #[test]
        fn two_runs_in_one_pass_with_one_gate_slot_place_one_gate() {
            let (mut rig, run, hub) = built();
            // A second run, issue 9, with its own session and its pull request #13.
            let second = RunId::new(REPO, 9, 1).unwrap();
            let (session2, workspace2) = (
                SessionId(uuid::Uuid::from_u128(0x6e5)),
                WorkspaceId(uuid::Uuid::from_u128(0xa22)),
            );
            rig.step(&second, &format!("approved {LABELED_AT}"));
            rig.moved(&second, State::Ready, State::Triage);
            rig.step(&second, "approval-rechecked");
            rig.write(
                &second,
                Event::SessionLaunched {
                    thread_id: session2.0.to_string(),
                    worktree: Some(workspace2.0.to_string()),
                    branch: Some("fac/9-1".into()),
                    model_instance: None,
                    model: Some("opus".into()),
                    model_options: None,
                    resumed_from: None,
                    base: Some("main".into()),
                    title: None,
                },
            );
            rig.moved(&second, State::Triage, State::Building);
            rig.write(
                &second,
                Event::PrOpened {
                    pr: 13,
                    url: "https://github.com/rawphp/softfac/pull/13".into(),
                },
            );
            rig.step(&second, "turn-read 1");
            hub.set_pull(
                13,
                HubPull {
                    head_ref: "fac/9-1".into(),
                    head: MOVED.into(),
                    ..draft_on(HEAD)
                },
            );
            let view = gate_machine(FleetView::builder())
                .session(session_record(SessionState::Idle))
                .turn(SessionTurn {
                    session: session(),
                    workspace: workspace(),
                    project: project_id(),
                    turn: 1,
                    prompts: 1,
                    question: false,
                })
                .session(Record {
                    owner: builder(),
                    version: 1,
                    deleted: false,
                    body: Session {
                        id: session2,
                        workspace: workspace2,
                        terminal: TerminalId(uuid::Uuid::from_u128(0x6e6)),
                        model: "opus".into(),
                        state: SessionState::Idle,
                        name: None,
                    },
                })
                .turn(SessionTurn {
                    session: session2,
                    workspace: workspace2,
                    project: project_id(),
                    turn: 1,
                    prompts: 1,
                    question: false,
                });
            rig.view = view.build();
            let mut slots = Slots {
                runs_free: 2,
                gates_free: 1,
            };
            rig.worker.on_create(Ok(gate_ws(1)));
            let first = advance(&rig.ctx(), &run, &shared(), &mut slots).unwrap();
            let again = advance(&rig.ctx(), &second, &shared(), &mut slots).unwrap();
            assert_eq!(first, GateStep::Running);
            assert_eq!(again, GateStep::Queued);
            assert_eq!(slots.gates_free, 0);
            assert_eq!(creates(&rig), 1);
            assert!(rig.events(&second).iter().all(|event| {
                event.data["step"]
                    .as_str()
                    .is_none_or(|step| !step.starts_with("gate-"))
            }));
        }

        #[test]
        fn gate_running_says_which_runs_hold_a_gate_slot() {
            let (mut rig, run, _hub) = built();
            assert!(!gate_running(&rig.events(&run)));
            place(&mut rig, &run, 1, 1);
            assert!(gate_running(&rig.events(&run)));
            ends(&mut rig, &run, 1, 1, 101, FAIL_TAIL);
            assert!(!gate_running(&rig.events(&run)));
            let (mut other, run2, _) = built();
            other.guard = Box::new(Lost);
            let _ = try_go(&other, &run2);
            // A request recorded and not yet answered holds a slot.
            assert!(gate_running(&other.events(&run2)));
        }
    }

    // ----- the runner's "every call can fail, and the gate is called again" family -----

    mod gate_every_call_can_fail_and_the_gate_is_called_again {
        use super::*;

        type Arrange = fn(&mut Rig, &Hub);

        /// Passes until the pass settles, the gate's terminal showing `code` and `tail` once
        /// it is placed. A lost lease or a failed call is the end of a pass, not of the test.
        fn complete(rig: &mut Rig, run: &RunId, code: i32, tail: &str) -> GateStep {
            for _ in 0..60 {
                match try_go(rig, run) {
                    Err(
                        RunError::LeaseLost(_)
                        | RunError::Tracker(_)
                        | RunError::Worker(_)
                        | RunError::Send(_),
                    ) => rig.guard = Box::new(AlwaysHeld(1)),
                    Err(error) => panic!("{error:?}"),
                    Ok(GateStep::Running) => {
                        show(rig, run, SessionState::Idle, 1, &[exited(1, code)]);
                        for _ in 0..3 {
                            rig.worker.on_tail(Ok(tail.into()));
                        }
                    }
                    Ok(step) => return step,
                }
            }
            panic!("the pass did not settle");
        }

        fn fresh(arrange: Arrange) -> (Rig, RunId, Arc<Hub>) {
            let (mut rig, run, hub) = built();
            arrange(&mut rig, &hub);
            for _ in 0..3 {
                rig.worker.on_create(Ok(gate_ws(1)));
            }
            (rig, run, hub)
        }

        /// The steps that mark the gate's own writes, which each exist once.
        fn markers(rig: &Rig, run: &RunId) -> Vec<String> {
            rig.steps(run)
                .into_iter()
                .filter(|step| {
                    ["gate-", "build-", "send-typed"]
                        .iter()
                        .any(|prefix| step.starts_with(prefix))
                })
                .map(|step| step.lines().next().unwrap_or_default().to_owned())
                .collect()
        }

        fn same_after_a_cut_at_every_check(arrange: Arrange, code: i32, tail: &str) {
            let (mut rig, run, _hub) = fresh(arrange);
            let step = complete(&mut rig, &run, code, tail);
            let (worker, comments, labels) = (calls(&rig), rig.sim.comments(), rig.sim.labels());
            let steps = markers(&rig, &run);
            for checks in 1..=30 {
                let (mut rig, run, _hub) = fresh(arrange);
                rig.guard = Box::new(HeldFor::new(checks));
                assert_eq!(complete(&mut rig, &run, code, tail), step, "{checks}");
                assert_eq!(calls(&rig), worker, "{checks}: worker calls");
                assert_eq!(rig.sim.comments(), comments, "{checks}: comments");
                assert_eq!(rig.sim.labels(), labels, "{checks}");
                assert_eq!(markers(&rig, &run), steps, "{checks}: recorded steps");
            }
        }

        fn same_after_a_failed_call(arrange: Arrange, code: i32, tail: &str, needles: &[&str]) {
            let (mut rig, run, _hub) = fresh(arrange);
            let step = complete(&mut rig, &run, code, tail);
            let (worker, comments, labels) = (calls(&rig), rig.sim.comments(), rig.sim.labels());
            for needle in needles {
                for lost in [false, true] {
                    let (mut rig, run, hub) = fresh(arrange);
                    if *needle == "pr view" || *needle == "check-runs" {
                        hub.fail(needle, 1);
                    } else if lost {
                        rig.sim.lose(needle, 1);
                    } else {
                        rig.sim.fail(needle, 1);
                    }
                    assert_eq!(
                        complete(&mut rig, &run, code, tail),
                        step,
                        "{needle} {lost}"
                    );
                    assert_eq!(calls(&rig), worker, "{needle} {lost}: worker calls");
                    assert_eq!(rig.sim.comments(), comments, "{needle} {lost}: comments");
                    assert_eq!(rig.sim.labels(), labels, "{needle} {lost}");
                }
            }
        }

        fn nothing(_: &mut Rig, _: &Hub) {}

        fn with_checks(rig: &mut Rig, hub: &Hub) {
            rig.config.required_checks = vec!["ci".into()];
            hub.set_checks(HEAD, &[("ci", "completed", Some("success"))]);
        }

        fn with_a_failed_check(rig: &mut Rig, hub: &Hub) {
            rig.config.required_checks = vec!["ci".into()];
            hub.set_checks(HEAD, &[("ci", "completed", Some("failure"))]);
        }

        fn no_retries(rig: &mut Rig, _: &Hub) {
            rig.config.limits.gate_retries = 0;
        }

        fn conflicting(_: &mut Rig, hub: &Hub) {
            hub.set_mergeable("CONFLICTING");
        }

        fn a_failed_check_and_no_retries(rig: &mut Rig, hub: &Hub) {
            with_a_failed_check(rig, hub);
            no_retries(rig, hub);
        }

        #[test]
        fn the_failed_check_with_no_retry_left_survives_a_cut_at_every_check() {
            same_after_a_cut_at_every_check(a_failed_check_and_no_retries, 0, PASS_TAIL);
            // The stuck decision resumed after a cut rebuilds the failed-check text it records.
            let stuck = |rig: &Rig, run: &RunId| {
                rig.events(run)
                    .into_iter()
                    .filter(|event| event.event == "stuck")
                    .map(|event| event.data)
                    .collect::<Vec<_>>()
            };
            let (mut rig, run, _hub) = fresh(a_failed_check_and_no_retries);
            complete(&mut rig, &run, 0, PASS_TAIL);
            let whole = stuck(&rig, &run);
            assert_eq!(whole.len(), 1);
            assert!(
                whole[0].to_string().contains("ci"),
                "the failed check is named: {whole:?}"
            );
            for checks in 1..=30 {
                let (mut rig, run, _hub) = fresh(a_failed_check_and_no_retries);
                rig.guard = Box::new(HeldFor::new(checks));
                complete(&mut rig, &run, 0, PASS_TAIL);
                assert_eq!(stuck(&rig, &run), whole, "{checks}");
            }
        }

        #[test]
        fn the_pass_path_survives_a_failure_of_each_call() {
            same_after_a_cut_at_every_check(nothing, 0, PASS_TAIL);
            same_after_a_failed_call(
                nothing,
                0,
                PASS_TAIL,
                &["pr ready", "pr comment", "issue edit", "issue comment"],
            );
        }

        #[test]
        fn a_pass_cut_short_at_each_write_and_repeated_writes_nothing_twice() {
            let (mut rig, run, _hub) = fresh(nothing);
            complete(&mut rig, &run, 0, PASS_TAIL);
            let comments = rig.sim.comments();
            assert_eq!(
                comments
                    .iter()
                    .filter(|c| c.contains("Factory gate: passed"))
                    .count(),
                1
            );
            for checks in 1..=30 {
                let (mut rig, run, _hub) = fresh(nothing);
                rig.guard = Box::new(HeldFor::new(checks));
                complete(&mut rig, &run, 0, PASS_TAIL);
                assert_eq!(
                    rig.sim
                        .comments()
                        .iter()
                        .filter(|c| c.contains("Factory gate: passed"))
                        .count(),
                    1,
                    "{checks}"
                );
                for (what, count) in [
                    ("create", creates(&rig)),
                    (
                        "end of sessions",
                        calls(&rig).iter().filter(|c| **c == end_of_build()).count(),
                    ),
                    (
                        "build archive",
                        calls(&rig)
                            .iter()
                            .filter(|c| **c == archive_of(workspace()))
                            .count(),
                    ),
                    (
                        "gate archive",
                        calls(&rig)
                            .iter()
                            .filter(|c| **c == archive_of(gate_ws(1)))
                            .count(),
                    ),
                ] {
                    assert_eq!(count, 1, "{checks}: {what}");
                }
            }
        }

        #[test]
        fn the_pass_path_with_required_checks_survives_a_failure_of_each_call() {
            same_after_a_cut_at_every_check(with_checks, 0, PASS_TAIL);
            same_after_a_failed_call(
                with_checks,
                0,
                PASS_TAIL,
                &["check-runs", "pr ready", "pr comment", "issue edit"],
            );
        }

        #[test]
        fn the_sent_back_path_survives_a_failure_of_each_call() {
            same_after_a_cut_at_every_check(nothing, 101, FAIL_TAIL);
            same_after_a_failed_call(nothing, 101, FAIL_TAIL, &["pr view"]);
        }

        #[test]
        fn the_failed_check_path_survives_a_failure_of_each_call() {
            same_after_a_cut_at_every_check(with_a_failed_check, 0, PASS_TAIL);
            same_after_a_failed_call(with_a_failed_check, 0, PASS_TAIL, &["check-runs"]);
        }

        #[test]
        fn the_exhausted_path_survives_a_failure_of_each_call() {
            same_after_a_cut_at_every_check(no_retries, 101, FAIL_TAIL);
            same_after_a_failed_call(no_retries, 101, FAIL_TAIL, &["issue comment", "issue edit"]);
        }

        #[test]
        fn a_call_cut_short_after_the_conflict_line_counts_the_attempt_once() {
            same_after_a_cut_at_every_check(conflicting, 0, PASS_TAIL);
            let (mut rig, run, _hub) = fresh(conflicting);
            rig.guard = Box::new(HeldFor::new(1));
            assert_eq!(
                complete(&mut rig, &run, 0, PASS_TAIL),
                GateStep::SentBack { attempt: 1 }
            );
            assert_eq!(event_count(&rig, &run, "gate_command"), 1);
            assert_eq!(event_count(&rig, &run, "gate_sent_back"), 1);
        }

        #[test]
        fn a_moved_head_survives_a_failure_of_each_call() {
            // The head moves between the pass that placed the gate and the one that read it.
            for checks in 1..=12 {
                let (mut rig, run, hub) = fresh(no_retries);
                rig.guard = Box::new(HeldFor::new(checks));
                assert_eq!(go_placing(&mut rig, &run), GateStep::Running, "{checks}");
                hub.set_head(MOVED);
                show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 0)]);
                rig.worker.on_tail(Ok(PASS_TAIL.into()));
                let mut step = None;
                for _ in 0..6 {
                    match try_go(&rig, &run) {
                        Err(_) => rig.guard = Box::new(AlwaysHeld(1)),
                        Ok(done) => {
                            step = Some(done);
                            break;
                        }
                    }
                }
                let step = step.expect("a settled pass");
                assert_stuck(&rig, &run, &step, "head-kept-moving", HEAD_KEPT_MOVING);
                assert_eq!(step_count(&rig, &run, GATE_HEAD_MOVED), 1, "{checks}");
            }
        }

        /// Passes until the gate is placed, whatever cuts a pass short.
        fn go_placing(rig: &mut Rig, run: &RunId) -> GateStep {
            for _ in 0..6 {
                match try_go(rig, run) {
                    Err(_) => rig.guard = Box::new(AlwaysHeld(1)),
                    Ok(step) => return step,
                }
            }
            panic!("the gate was never placed");
        }
    }

    // ----- review cycle 1 -----

    /// Round two of a run: round one passed, the run went back to `building` for feedback, and
    /// the session reported again after turn 2.
    fn second_round(rig: &mut Rig, run: &RunId, hub: &Hub) {
        assert_eq!(gated(rig, run, 0, PASS_TAIL), GateStep::Passed { pr: 12 });
        rig.moved(run, State::PrOpen, State::Building);
        rig.sim.set_labels(&["factory:building"]);
        // The feedback round has a session of its own.
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
                messages::reminder(),
            )
            .unwrap();
        reported_again(rig, run, hub, 2);
    }

    mod review_a_round_has_its_own_budget {
        use super::*;

        #[test]
        fn a_second_rounds_first_gate_is_not_stuck_with_no_retries() {
            let (mut rig, run, hub) = built();
            rig.config.limits.gate_retries = 0;
            second_round(&mut rig, &run, &hub);
            rig.worker.on_create(Ok(gate_ws(2)));
            assert_eq!(go(&rig, &run), GateStep::Running);
            assert!(has_step(&rig, &run, &format!("{GATE_REQUESTED}2 2 {HEAD}")));
        }

        #[test]
        fn a_second_round_that_fails_once_and_moves_once_within_gate_retries_still_passes() {
            let (mut rig, run, hub) = built();
            rig.config.limits.gate_retries = 2;
            second_round(&mut rig, &run, &hub);
            place(&mut rig, &run, 2, 2);
            assert_eq!(
                ends(&mut rig, &run, 2, 2, 101, FAIL_TAIL),
                GateStep::SentBack { attempt: 2 }
            );
            reported_again(&mut rig, &run, &hub, 3);
            place(&mut rig, &run, 3, 3);
            hub.set_head(MOVED);
            assert_eq!(ends(&mut rig, &run, 3, 3, 0, PASS_TAIL), GateStep::NotReady);
            rig.worker.on_create(Ok(gate_ws(4)));
            assert_eq!(go(&rig, &run), GateStep::Running);
            show(&mut rig, &run, SessionState::Idle, 3, &[running(4)]);
            assert_eq!(
                ends(&mut rig, &run, 3, 4, 0, PASS_TAIL),
                GateStep::Passed { pr: 12 }
            );
        }

        #[test]
        fn a_second_round_that_moves_twice_with_one_retry_ends_stuck() {
            let (mut rig, run, hub) = built();
            rig.config.limits.gate_retries = 1;
            second_round(&mut rig, &run, &hub);
            place(&mut rig, &run, 2, 2);
            hub.set_head(MOVED);
            assert_eq!(ends(&mut rig, &run, 2, 2, 0, PASS_TAIL), GateStep::NotReady);
            rig.worker.on_create(Ok(gate_ws(3)));
            assert_eq!(go(&rig, &run), GateStep::Running);
            hub.set_head(HEAD);
            show(&mut rig, &run, SessionState::Idle, 2, &[running(3)]);
            let step = ends(&mut rig, &run, 2, 3, 0, PASS_TAIL);
            assert_stuck_in(
                &rig,
                &run,
                &step,
                "head-kept-moving",
                HEAD_KEPT_MOVING,
                true,
            );
        }

        #[test]
        fn a_second_rounds_gate_is_named_above_every_earlier_one_and_adopts_nothing() {
            let (mut rig, run, hub) = built();
            second_round(&mut rig, &run, &hub);
            // Round one's gate is still in the view, exited 0.
            show(&mut rig, &run, SessionState::Idle, 2, &[exited(1, 0)]);
            rig.worker.on_create(Ok(gate_ws(2)));
            assert_eq!(go(&rig, &run), GateStep::Running);
            let Call::Create { request, .. } = calls(&rig).pop().unwrap() else {
                panic!("expected a create");
            };
            assert_eq!(request.name, "fac/8-1-g2");
            assert_eq!(step_count(&rig, &run, "gate-passed "), 1);
            assert_eq!(
                calls(&rig)
                    .iter()
                    .filter(|call| matches!(call, Call::Tail { .. }))
                    .count(),
                1,
                "only round one's own gate was read"
            );
        }
    }

    mod review_nothing_is_marked_ready_on_a_head_that_was_not_gated {
        use super::*;

        #[test]
        fn a_pass_cut_short_after_it_was_recorded_whose_head_then_moves_marks_nothing_ready() {
            let (mut rig, run, hub) = built();
            place(&mut rig, &run, 1, 1);
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 0)]);
            rig.worker.on_tail(Ok(PASS_TAIL.into()));
            rig.sim.fail("pr ready", 1);
            assert!(matches!(try_go(&rig, &run), Err(RunError::Tracker(_))));
            assert!(has_step(&rig, &run, "gate-passed "));
            hub.set_head(MOVED);
            assert_eq!(go(&rig, &run), GateStep::NotReady);
            assert!(hub.lock().pulls[&12].draft);
            assert_eq!(rig.sim.labels(), ["factory:building"]);
            assert!(has_step(&rig, &run, GATE_HEAD_MOVED));
            rig.worker.on_create(Ok(gate_ws(2)));
            assert_eq!(go(&rig, &run), GateStep::Running);
            assert!(has_step(
                &rig,
                &run,
                &format!("{GATE_REQUESTED}2 2 {MOVED}")
            ));
        }

        #[test]
        fn the_same_with_required_checks_that_were_already_read_as_passed() {
            let (mut rig, run, hub) = built();
            rig.config.required_checks = vec!["ci".into()];
            hub.set_checks(HEAD, &[("ci", "completed", Some("success"))]);
            place(&mut rig, &run, 1, 1);
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 0)]);
            rig.worker.on_tail(Ok(PASS_TAIL.into()));
            rig.sim.fail("pr ready", 1);
            assert!(matches!(try_go(&rig, &run), Err(RunError::Tracker(_))));
            assert_eq!(event_count(&rig, &run, "checks_read"), 1);
            hub.set_head(MOVED);
            assert_eq!(go(&rig, &run), GateStep::NotReady);
            assert!(hub.lock().pulls[&12].draft);
            assert_eq!(rig.sim.labels(), ["factory:building"]);
        }

        #[test]
        fn a_pr_that_a_crashed_gate_already_marked_ready_is_still_the_runs_pr() {
            let (mut rig, run, hub) = built();
            place(&mut rig, &run, 1, 1);
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 0)]);
            rig.worker.on_tail(Ok(PASS_TAIL.into()));
            rig.sim.lose("pr ready", 1);
            assert!(matches!(try_go(&rig, &run), Err(RunError::Tracker(_))));
            assert!(!hub.lock().pulls[&12].draft);
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
            assert_eq!(rig.sim.labels(), ["factory:pr-open"]);
        }
    }

    mod review_every_gate_workspace_is_archived {
        use super::*;

        #[test]
        fn a_create_that_failed_after_the_worker_made_the_workspace_has_it_archived() {
            let (mut rig, run, _hub) = built();
            rig.worker
                .on_create(Err(WorkerError::Unreachable("the call timed out".into())));
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 1 });
            // The worker made it all the same, and its machine is there.
            show(&mut rig, &run, SessionState::Idle, 1, &[running(1)]);
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 0 });
            assert_eq!(archived(&rig), vec![gate_ws(1)]);
            assert!(!gate_running(&rig.events(&run)));
        }

        #[test]
        fn reaching_pr_open_archives_a_gate_that_could_not_run_too() {
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            show(
                &mut rig,
                &run,
                SessionState::Idle,
                1,
                &[G {
                    k: 1,
                    state: TerminalState::Exited,
                    code: None,
                }],
            );
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 1 });
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 0 });
            show(&mut rig, &run, SessionState::Idle, 1, &[]);
            place(&mut rig, &run, 1, 2);
            assert_eq!(
                ends(&mut rig, &run, 1, 2, 0, PASS_TAIL),
                GateStep::Passed { pr: 12 }
            );
            let mut gone = archived(&rig);
            gone.sort();
            let mut all = vec![workspace(), gate_ws(1), gate_ws(2)];
            all.sort();
            assert_eq!(gone, all);
            let order = calls(&rig);
            let first = order
                .iter()
                .position(|c| *c == archive_of(gate_ws(1)))
                .unwrap();
            let second_create = order
                .iter()
                .rposition(|c| matches!(c, Call::Create { .. }))
                .unwrap();
            assert!(first < second_create, "g1 is archived before g2 is placed");
        }

        #[test]
        fn a_gate_in_flight_is_archived_when_the_run_goes_stuck() {
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            rig.write(&run, Event::StopRequested { by: "tom".into() });
            let step = go(&rig, &run);
            assert!(matches!(step, GateStep::Stuck { .. }));
            assert!(archived(&rig).contains(&gate_ws(1)));
            assert!(!gate_running(&rig.events(&run)));
        }

        #[test]
        fn a_gate_in_flight_is_archived_when_the_issue_is_closed() {
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            rig.sim.set_closed(true);
            assert_eq!(go(&rig, &run), GateStep::Abandoned);
            assert!(archived(&rig).contains(&gate_ws(1)));
            assert!(!gate_running(&rig.events(&run)));
        }

        #[test]
        fn a_gate_in_flight_is_archived_when_the_runs_own_merge_ends_it() {
            let (mut rig, run, hub) = built();
            place(&mut rig, &run, 1, 1);
            rig.sim.set_closed(true);
            hub.set_state("MERGED");
            assert_eq!(go(&rig, &run), GateStep::Ended);
            assert!(archived(&rig).contains(&gate_ws(1)));
            assert!(!gate_running(&rig.events(&run)));
        }

        #[test]
        fn a_created_gate_that_vanished_from_the_fleet_does_not_hold_its_slot_forever() {
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            show(&mut rig, &run, SessionState::Idle, 1, &[]);
            for pass in 1..=5 {
                assert_eq!(go(&rig, &run), GateStep::Running, "pass {pass}");
            }
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 1 });
            assert!(!gate_running(&rig.events(&run)));
            assert_eq!(sends(&rig), vec![]);
            // Its id is on record: it is archived by call before another gate is placed,
            // whatever the view shows.
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 0 });
            assert_eq!(archived(&rig), vec![gate_ws(1)]);
        }

        #[test]
        fn an_archive_that_failed_otherwise_is_asked_again_with_the_verdict_kept() {
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 101)]);
            rig.worker.on_tail(Ok(FAIL_TAIL.into()));
            rig.worker
                .on_archive(Err(WorkerError::Failed("the worktree is busy".into())));
            assert!(matches!(try_go(&rig, &run), Err(RunError::Worker(_))));
            assert_eq!(go(&rig, &run), GateStep::SentBack { attempt: 1 });
            assert_eq!(archived(&rig), vec![gate_ws(1), gate_ws(1)]);
            assert_eq!(
                calls(&rig)
                    .iter()
                    .filter(|call| matches!(call, Call::Tail { .. }))
                    .count(),
                1
            );
        }
    }

    mod review_the_verdict_does_not_share_a_step_with_the_tail {
        use super::*;

        /// The gate's output events, the tail of each in its own field.
        fn tail_events(rig: &Rig, run: &RunId) -> Vec<String> {
            rig.events(run)
                .into_iter()
                .filter(|event| {
                    event.event == "gate_cleanup_failed" && event.data["dir"] == "fac/8-1-g1"
                })
                .map(|event| event.data["error"].as_str().unwrap().to_owned())
                .collect()
        }

        #[test]
        fn a_tail_of_more_than_64_kb_still_settles_the_gate_once() {
            let (mut rig, run, _hub) = built();
            let tail = format!(
                "$ cargo test\n{}setup failed: cargo test exited 101\n",
                "x\n".repeat(100_000)
            );
            assert_eq!(
                gated(&mut rig, &run, 101, &tail),
                GateStep::SentBack { attempt: 1 }
            );
            assert_eq!(step_count(&rig, &run, "gate-ended 1 101"), 1);
            let kept = tail_events(&rig, &run);
            assert_eq!(kept.len(), 1);
            assert!(kept[0].len() <= 64 * 1024, "{}", kept[0].len());
            // Later passes do not read or record it again.
            show(&mut rig, &run, SessionState::Idle, 1, &[]);
            assert_eq!(go(&rig, &run), GateStep::NotReady);
            assert_eq!(tail_events(&rig, &run).len(), 1);
            assert_eq!(
                calls(&rig)
                    .iter()
                    .filter(|call| matches!(call, Call::Tail { .. }))
                    .count(),
                1
            );
        }

        #[test]
        fn a_tail_that_the_hiding_makes_longer_still_settles_the_gate_once() {
            let mut rig = Rig::new();
            let (dir, store) = crate::factory::ledger::testing::open_store();
            rig.ledger = crate::factory::ledger::Ledger::new(
                store,
                MachineId(uuid::Uuid::from_u128(0x1)),
                1,
                Arc::new(|text: &str| text.replace('#', "HIDDEN-SECRET-")),
            )
            .with_wall(crate::factory::ledger::testing::ticking(
                crate::factory::ledger::testing::T0,
                1000,
            ));
            rig._dir = dir;
            let (mut rig, run, _hub) = built_on(rig);
            let tail = format!(
                "$ cargo test\n{}\nsetup failed: cargo test exited 101\n",
                "#".repeat(30_000)
            );
            assert_eq!(
                gated(&mut rig, &run, 101, &tail),
                GateStep::SentBack { attempt: 1 }
            );
            assert_eq!(step_count(&rig, &run, "gate-ended 1 101"), 1);
            assert_eq!(tail_events(&rig, &run).len(), 1);
            assert_eq!(
                calls(&rig)
                    .iter()
                    .filter(|call| matches!(call, Call::Tail { .. }))
                    .count(),
                1
            );
        }

        #[test]
        fn a_multibyte_tail_is_cut_on_a_character_boundary() {
            let (mut rig, run, _hub) = built();
            let tail = format!(
                "$ cargo test\n{}\nsetup failed: cargo test exited 101\n",
                "é".repeat(70_000)
            );
            assert_eq!(
                gated(&mut rig, &run, 101, &tail),
                GateStep::SentBack { attempt: 1 }
            );
            let kept = tail_events(&rig, &run);
            assert_eq!(kept.len(), 1);
            assert!(kept[0].len() <= 64 * 1024);
            assert!(kept[0].starts_with('é') || kept[0].starts_with("\nsetup"));
        }
    }

    mod review_the_checks_wait_counts_on_the_sleep_counting_clock {
        use super::*;
        use crate::clock::FakeClock;

        const BUSY: &[(&str, &str, Option<&str>)] = &[("ci", "in_progress", None)];

        fn waiting(rig: &Rig, run: &RunId) -> Vec<String> {
            rig.steps(run)
                .into_iter()
                .filter(|step| step.starts_with("checks-waiting "))
                .collect()
        }

        fn pending(rig: &mut Rig, run: &RunId, hub: &Hub) {
            rig.config.required_checks = vec!["ci".into()];
            hub.set_checks(HEAD, BUSY);
            assert_eq!(gated(rig, run, 0, PASS_TAIL), GateStep::ChecksPending);
        }

        fn after(rig: &Rig, run: &RunId, minutes: u64) -> GateStep {
            rig.clock.advance(Duration::from_secs(minutes * 60));
            go(rig, run)
        }

        #[test]
        fn the_wait_is_recorded_with_the_generation_and_the_clock_and_counted_from_it() {
            let (mut rig, run, hub) = built();
            pending(&mut rig, &run, &hub);
            assert_eq!(waiting(&rig, &run), ["checks-waiting 1 0"]);
            assert_eq!(after(&rig, &run, 29), GateStep::ChecksPending);
            assert_eq!(waiting(&rig, &run).len(), 1, "one start, not one per pass");
            let step = after(&rig, &run, 2);
            assert_stuck(
                &rig,
                &run,
                &step,
                "checks-timed-out",
                "the required checks did not finish within 30 minutes",
            );
        }

        #[test]
        fn a_takeover_restarts_the_wait_and_never_ends_it_early() {
            let (mut rig, run, hub) = built();
            pending(&mut rig, &run, &hub);
            assert_eq!(after(&rig, &run, 20), GateStep::ChecksPending);
            rig.guard = Box::new(AlwaysHeld(2));
            // 40 minutes since the first start, but the wait is a new coordinator's.
            assert_eq!(after(&rig, &run, 20), GateStep::ChecksPending);
            assert_eq!(waiting(&rig, &run).len(), 2);
            assert!(waiting(&rig, &run)[1].starts_with("checks-waiting 2 "));
            assert_eq!(after(&rig, &run, 29), GateStep::ChecksPending);
            assert!(matches!(after(&rig, &run, 2), GateStep::Stuck { .. }));
        }

        #[test]
        fn a_clock_reading_below_the_recorded_one_restarts_the_wait() {
            let (mut rig, run, hub) = built();
            rig.clock.advance(Duration::from_secs(10 * 60));
            pending(&mut rig, &run, &hub);
            assert_eq!(waiting(&rig, &run), ["checks-waiting 1 600000"]);
            // A reboot reset the clock.
            rig.clock = FakeClock::new();
            assert_eq!(after(&rig, &run, 5), GateStep::ChecksPending);
            assert_eq!(waiting(&rig, &run)[1], "checks-waiting 1 300000");
            assert_eq!(after(&rig, &run, 29), GateStep::ChecksPending);
            assert!(matches!(after(&rig, &run, 2), GateStep::Stuck { .. }));
        }

        #[test]
        fn the_wall_clock_is_not_read_for_the_wait() {
            // The ledger stamps its events weeks behind the system's wall clock: a wait
            // measured on it would be over at once.
            let (mut rig, run, hub) = built();
            pending(&mut rig, &run, &hub);
            assert_eq!(after(&rig, &run, 1), GateStep::ChecksPending);
            assert!(!rig.finished(&run));
        }
    }

    mod review_putting_the_pr_back_to_draft {
        use super::*;

        /// A tracker whose call to put the pull request back to draft fails with `error`,
        /// after `effect` happened on GitHub; later reads fail while `read_fails`.
        struct Draft {
            hub: Arc<Hub>,
            error: fn() -> TrackerError,
            effect: fn(&Hub),
            read_fails: bool,
            failed: std::sync::atomic::AtomicBool,
        }

        impl Tracker for Draft {
            fn gh(&self, args: &[String]) -> Result<String, TrackerError> {
                use std::sync::atomic::Ordering;
                if args.iter().any(|arg| arg == "--undo") {
                    (self.effect)(&self.hub);
                    self.failed.store(true, Ordering::SeqCst);
                    return Err((self.error)());
                }
                if self.read_fails
                    && self.failed.load(Ordering::SeqCst)
                    && args.first().map(String::as_str) == Some("pr")
                    && args.get(1).map(String::as_str) == Some("view")
                {
                    return Err(TrackerError::Other {
                        kind: OtherKind::NoAnswer,
                        message: "`gh pr view` exited with status 1: HTTP 502: Bad Gateway".into(),
                    });
                }
                self.hub.gh(args)
            }
        }

        fn failed_with(message: &'static str) -> fn() -> TrackerError {
            match message {
                "boom" => || TrackerError::Other {
                    kind: OtherKind::Failed,
                    message: "`gh pr ready 12` exited with status 1: boom".into(),
                },
                // `github::gh` gives these the kind `NoAnswer`: GitHub never answered.
                "502" => || TrackerError::Other {
                    kind: OtherKind::NoAnswer,
                    message: "`gh pr ready 12` exited with status 1: HTTP 502: Bad Gateway".into(),
                },
                "connecting" => || TrackerError::Other {
                    kind: OtherKind::NoAnswer,
                    message:
                        "`gh pr ready 12` exited with status 1: error connecting to api.github.com"
                            .into(),
                },
                "signal" => || TrackerError::Other {
                    kind: OtherKind::NoAnswer,
                    message: "`gh pr ready 12` exited with status -1: killed".into(),
                },
                "404" => || TrackerError::NotFound {
                    command: "gh pr ready".into(),
                    status: 1,
                    stderr: "HTTP 404: Not Found".into(),
                },
                _ => || TrackerError::Timeout {
                    command: "gh pr ready".into(),
                    seconds: 30,
                },
            }
        }

        /// A failed gate on a pull request someone marked ready, whose put-back call fails.
        fn drafting(
            error: fn() -> TrackerError,
            effect: fn(&Hub),
            read_fails: bool,
        ) -> (Rig, RunId, Arc<Hub>) {
            let (mut rig, run, hub) = built();
            place(&mut rig, &run, 1, 1);
            hub.set_draft(false);
            rig.tracker = Hiding::new(
                Arc::new(Draft {
                    hub: hub.clone(),
                    error,
                    effect,
                    read_fails,
                    failed: std::sync::atomic::AtomicBool::new(false),
                }),
                Arc::new(|text: &str| text.to_string()),
            );
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 101)]);
            rig.worker.on_tail(Ok(FAIL_TAIL.into()));
            (rig, run, hub)
        }

        fn nothing(_: &Hub) {}

        #[test]
        fn a_failure_with_the_pull_request_still_not_a_draft_is_stuck() {
            for name in ["boom", "404"] {
                let (rig, run, _hub) = drafting(failed_with(name), nothing, false);
                let step = go(&rig, &run);
                assert_stuck(
                    &rig,
                    &run,
                    &step,
                    "pr-not-bound",
                    "the session reported PR #12 but it is not a draft",
                );
            }
        }

        #[test]
        fn a_failure_that_got_no_answer_from_github_is_an_error_not_stuck() {
            for name in ["502", "connecting", "signal", "timeout"] {
                let (rig, run, _hub) = drafting(failed_with(name), nothing, false);
                assert!(
                    matches!(try_go(&rig, &run), Err(RunError::Tracker(_))),
                    "{name}"
                );
                assert!(!rig.finished(&run), "{name}");
                assert_eq!(rig.sim.labels(), ["factory:building"], "{name}");
                assert_eq!(sends(&rig), vec![], "{name}");
            }
        }

        #[test]
        fn a_failure_whose_second_look_fails_is_an_error_not_stuck() {
            let (rig, run, _hub) = drafting(failed_with("boom"), nothing, true);
            assert!(matches!(try_go(&rig, &run), Err(RunError::Tracker(_))));
            assert!(!rig.finished(&run));
        }

        #[test]
        fn a_failure_after_which_the_pull_request_reads_as_a_draft_goes_on() {
            fn drafts(hub: &Hub) {
                hub.set_draft(true);
            }
            let (rig, run, _hub) = drafting(failed_with("502"), drafts, false);
            assert_eq!(go(&rig, &run), GateStep::SentBack { attempt: 1 });
            assert_eq!(event_count(&rig, &run, "pr_set_draft"), 1);
        }

        #[test]
        fn a_pull_request_merged_before_the_call_ends_the_run() {
            fn merges(hub: &Hub) {
                hub.set_state("MERGED");
            }
            let (rig, run, _hub) = drafting(failed_with("boom"), merges, false);
            assert_eq!(go(&rig, &run), GateStep::Ended);
            assert!(rig.finished(&run));
            assert_eq!(sends(&rig), vec![]);
        }
    }

    // ----- the runner's cases with a counterpart here, ported last -----

    mod gate_ported_binding_and_the_head {
        use super::*;

        fn launched(
            branch: Option<&str>,
            resumed: Option<&str>,
            base: Option<&str>,
        ) -> (Rig, RunId, Arc<Hub>) {
            built_launch(Rig::new(), branch, resumed, base)
        }

        #[test]
        fn a_pr_on_the_launch_branch_of_the_sessions_worktree_is_gated() {
            let (mut rig, run, hub) = launched(Some("fac/8-9"), None, Some("main"));
            hub.edit(12, |pull| pull.head_ref = "fac/8-9".into());
            assert_eq!(
                gated(&mut rig, &run, 0, PASS_TAIL),
                GateStep::Passed { pr: 12 }
            );
        }

        #[test]
        fn binds_a_pull_request_on_the_branch_an_earlier_attempt_worked_on_at_gate_time_too() {
            let (mut rig, run, hub) = built();
            hub.edit(12, |pull| pull.head_ref = "t3code/old".into());
            rig.write(
                &run,
                Event::BranchBound {
                    pr: 12,
                    branch: "t3code/old".into(),
                },
            );
            assert_eq!(
                gated(&mut rig, &run, 0, PASS_TAIL),
                GateStep::Passed { pr: 12 }
            );
        }

        #[test]
        fn binds_a_pull_request_on_a_branch_the_session_made_at_gate_time_too() {
            let (mut rig, run, hub) = built();
            hub.edit(12, |pull| pull.head_ref = "t3/kpi-push-screen".into());
            rig.write(
                &run,
                Event::BranchBound {
                    pr: 12,
                    branch: "t3/kpi-push-screen".into(),
                },
            );
            assert_eq!(
                gated(&mut rig, &run, 0, PASS_TAIL),
                GateStep::Passed { pr: 12 }
            );
        }

        #[test]
        fn a_session_with_no_branch_at_gate_time_fails_closed() {
            let (rig, run, _hub) = launched(None, None, Some("main"));
            let step = go(&rig, &run);
            assert_stuck(
                &rig,
                &run,
                &step,
                "pr-not-bound",
                "the session reported PR #12 but the session has no branch",
            );
            assert_eq!(creates(&rig), 0);
        }

        #[test]
        fn is_accepted_from_the_branch_the_run_resumed_from() {
            let (mut rig, run, hub) = launched(Some(BRANCH), Some("t3code/old"), Some("main"));
            hub.edit(12, |pull| pull.head_ref = "t3code/old".into());
            assert_eq!(
                gated(&mut rig, &run, 0, PASS_TAIL),
                GateStep::Passed { pr: 12 }
            );
            assert_eq!(creates(&rig), 1, "the gate ran on the pull request");
        }

        #[test]
        fn a_checkout_that_reports_the_head_in_capitals_is_at_the_same_commit() {
            let (mut rig, run, hub) = built();
            hub.set_head(&HEAD.to_uppercase());
            assert_eq!(
                gated(&mut rig, &run, 0, PASS_TAIL),
                GateStep::Passed { pr: 12 }
            );
            assert!(has_step(&rig, &run, &format!("{GATE_REQUESTED}1 1 {HEAD}")));
        }
    }

    mod gate_ported_the_launch_base {
        use super::*;

        #[test]
        fn a_run_with_no_base_on_its_launch_line_keeps_the_projects_base() {
            let (rig, run, hub) = built_launch(Rig::new(), Some(BRANCH), None, None);
            assert_eq!(go(&rig, &run), GateStep::Running);
            assert!(
                hub.reads()
                    .iter()
                    .any(|read| read.contains("git/ref/heads/main"))
            );
        }

        #[test]
        fn gates_against_that_bases_tip_and_a_move_of_any_base_does_not_re_gate() {
            let (mut rig, run, hub) =
                built_launch(Rig::new(), Some(BRANCH), None, Some("release/2"));
            hub.edit(12, |pull| pull.base = "release/2".into());
            hub.set_tip("release/2", MOVED);
            place(&mut rig, &run, 1, 1);
            assert!(has_step(
                &rig,
                &run,
                &format!("{GATE_REQUESTED}1 1 {HEAD} {MOVED}")
            ));
            // Every base moves while the commands ran: the pass stands.
            hub.set_tip("release/2", HEAD);
            hub.set_tip("main", HEAD);
            assert_eq!(
                ends(&mut rig, &run, 1, 1, 0, PASS_TAIL),
                GateStep::Passed { pr: 12 }
            );
            assert_eq!(creates(&rig), 1);
        }

        #[test]
        fn a_base_that_moved_while_the_commands_ran_does_not_rerun_the_gate() {
            let (mut rig, run, hub) = built();
            place(&mut rig, &run, 1, 1);
            hub.set_tip("main", MOVED);
            assert_eq!(
                ends(&mut rig, &run, 1, 1, 0, PASS_TAIL),
                GateStep::Passed { pr: 12 }
            );
            assert_eq!(creates(&rig), 1);
        }
    }

    mod gate_ported_the_decision_that_the_gate_passed_is_in_the_ledger {
        use super::*;

        /// A pass recorded and the pass cut short before anything was marked ready.
        fn passed_then_died() -> (Rig, RunId, Arc<Hub>) {
            let (mut rig, run, hub) = built();
            place(&mut rig, &run, 1, 1);
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 0)]);
            rig.worker.on_tail(Ok(PASS_TAIL.into()));
            rig.worker
                .on_end_sessions(Err(WorkerError::Failed("the machine is busy".into())));
            assert!(matches!(try_go(&rig, &run), Err(RunError::Worker(_))));
            (rig, run, hub)
        }

        #[test]
        fn a_recorded_pass_is_finished_without_the_commands_and_stops_the_session_once() {
            let (rig, run, hub) = passed_then_died();
            // The pull request was marked ready before the crash.
            hub.set_draft(false);
            let before = calls(&rig).len();
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
            assert_eq!(
                calls(&rig).split_off(before),
                vec![end_of_build(), archive_of(workspace())]
            );
            assert_eq!(creates(&rig), 1);
            assert_eq!(rig.sim.labels(), ["factory:pr-open"]);
        }

        #[test]
        fn the_rebuilt_results_are_the_last_full_run_of_the_passed_attempt() {
            let (mut rig, run, _hub) = built();
            rig.config.gate = vec!["cargo fmt".into(), "cargo test".into()];
            let line = |command: &str, ms: u64| Event::GateCommand {
                attempt: 1,
                command: command.into(),
                exit_code: 0,
                duration_ms: ms,
            };
            // A first run died after one line was written; the rerun passed and wrote them all.
            rig.write(&run, line("cargo fmt", 999));
            rig.write(&run, line("cargo fmt", 1_234));
            rig.write(&run, line("cargo test", 5_678));
            rig.step(&run, &format!("gate-passed {HEAD} 2"));
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
            let comment = rig
                .sim
                .comments()
                .into_iter()
                .find(|comment| comment.contains("Factory gate: passed"))
                .expect("the results");
            assert!(comment.contains("`cargo fmt` | 0 | 1.2 s"), "{comment}");
            assert!(comment.contains("`cargo test` | 0 | 5.7 s"), "{comment}");
            assert!(!comment.contains("1.0 s"), "{comment}");
        }

        #[test]
        fn a_step_that_is_not_exactly_gate_passed_and_a_commit_id_is_ignored() {
            for text in [
                "gate-passed",
                "gate-passed ",
                "gate-passed 4f1c2d3",
                "gate-passed zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
                "gate-passed 4f1c2d3e4a5b60718293a4b5c6d7e8f901234567",
                "gate-passed 4f1c2d3e4a5b60718293a4b5c6d7e8f901234567 0",
                "gate-passed 4f1c2d3e4a5b60718293a4b5c6d7e8f901234567 02",
                "gate-passed 4f1c2d3e4a5b60718293a4b5c6d7e8f901234567 2 3",
                "gate-passed 4f1c2d3e4a5b60718293a4b5c6d7e8f901234567 two",
                "Gate-passed 4f1c2d3e4a5b60718293a4b5c6d7e8f901234567 2",
                "xgate-passed 4f1c2d3e4a5b60718293a4b5c6d7e8f901234567 2",
            ] {
                let (rig, run, hub) = built();
                rig.step(&run, text);
                hub.set_draft(false);
                // No pass is on record: the gate is placed, nothing is marked ready.
                assert_eq!(go(&rig, &run), GateStep::Running, "{text}");
                assert!(
                    !rig.sim.writes().iter().any(|w| w.contains("pr ready 12")),
                    "{text}"
                );
            }
        }

        #[test]
        fn advance_says_a_run_that_is_already_pr_open_passed_and_writes_nothing() {
            let (mut rig, run, hub) = built();
            gated(&mut rig, &run, 0, PASS_TAIL);
            let events = rig.events(&run).len();
            let reads = hub.reads().len();
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
            assert_eq!(rig.events(&run).len(), events);
            assert_eq!(hub.reads().len(), reads);
            assert_eq!(rig.events(&run).last().unwrap().event, "step_changed");
        }

        #[test]
        fn a_pr_merged_while_the_daemon_was_down_does_not_undo_a_recorded_pass() {
            let mut cut_after_the_pass = 0;
            for checks in 1..=12 {
                let (mut rig, run, hub) = built();
                place(&mut rig, &run, 1, 1);
                show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 0)]);
                rig.worker.on_tail(Ok(PASS_TAIL.into()));
                rig.guard = Box::new(HeldFor::new(checks));
                if try_go(&rig, &run).is_ok() {
                    continue;
                }
                rig.guard = Box::new(AlwaysHeld(1));
                if !has_step(&rig, &run, "gate-passed ") {
                    continue;
                }
                cut_after_the_pass += 1;
                hub.set_state("MERGED");
                hub.set_draft(false);
                let comments = rig.sim.comments().len();
                assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 }, "{checks}");
                // Nothing is posted on the merged pull request, nobody is told it is stuck.
                assert!(rig.sim.comments().len() <= comments + 1, "{checks}");
                assert!(
                    rig.sim
                        .comments()
                        .iter()
                        .all(|comment| !comment.contains("could not finish")),
                    "{checks}"
                );
                assert_eq!(rig.sim.labels(), ["factory:pr-open"], "{checks}");
            }
            assert!(cut_after_the_pass > 0);
        }

        #[test]
        fn a_pr_that_is_gone_while_the_daemon_was_down_does_not_undo_a_recorded_pass_either() {
            let (rig, run, hub) = passed_then_died();
            hub.remove_pull();
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
            assert_eq!(rig.sim.labels(), ["factory:pr-open"]);
        }
    }

    mod gate_ported_sent_back_cases {
        use super::*;

        fn sent_back_once() -> (Rig, RunId, Arc<Hub>) {
            let (mut rig, run, hub) = built();
            assert_eq!(
                gated(&mut rig, &run, 101, FAIL_TAIL),
                GateStep::SentBack { attempt: 1 }
            );
            (rig, run, hub)
        }

        #[test]
        fn a_send_that_reached_the_session_whose_next_run_asked_questions_is_never_gated() {
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            rig.worker.on_send(Err(SendRefused::Failed {
                reason: "the answer was lost".into(),
                prompts: Some(1),
            }));
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 101)]);
            rig.worker.on_tail(Ok(FAIL_TAIL.into()));
            assert!(matches!(try_go(&rig, &run), Err(RunError::Send(_))));
            // The line was typed; the session's next run asked questions, so it never reported.
            rig.worker.on_send(Err(SendRefused::CountRose { now: 2 }));
            show(&mut rig, &run, SessionState::Idle, 2, &[]);
            assert_eq!(go(&rig, &run), GateStep::SentBack { attempt: 1 });
            assert_eq!(go(&rig, &run), GateStep::NotReady);
            assert_eq!(creates(&rig), 1);
        }

        #[test]
        fn advance_waits_after_a_sent_back_gate_even_for_a_run_that_has_no_ordinal_yet() {
            let (mut rig, run, _hub) = sent_back_once();
            show(&mut rig, &run, SessionState::Idle, 0, &[]);
            assert_eq!(go(&rig, &run), GateStep::NotReady);
            assert_eq!(creates(&rig), 1);
        }

        #[test]
        fn a_new_run_that_ends_without_a_done_line_after_a_sent_back_gate_is_read_like_any_other() {
            let (mut rig, run, _hub) = sent_back_once();
            // A newer turn that reported no pull request is the session's, not the gate's.
            show(&mut rig, &run, SessionState::Idle, 2, &[]);
            assert_eq!(go(&rig, &run), GateStep::NotReady);
            assert_eq!(creates(&rig), 1);
        }

        #[test]
        fn an_archive_that_fails_before_the_gate_gives_up_is_an_error_and_the_next_call_looks_again()
         {
            let (mut rig, run, _hub) = built();
            rig.config.limits.gate_retries = 0;
            place(&mut rig, &run, 1, 1);
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 101)]);
            rig.worker.on_tail(Ok(FAIL_TAIL.into()));
            rig.worker
                .on_end_sessions(Err(WorkerError::Failed("the machine is busy".into())));
            assert!(matches!(try_go(&rig, &run), Err(RunError::Worker(_))));
            assert!(!rig.finished(&run));
            assert_eq!(rig.sim.labels(), ["factory:building"]);
            let step = go(&rig, &run);
            assert!(matches!(step, GateStep::Stuck { .. }));
            assert!(rig.finished(&run));
        }
    }

    mod gate_ported_every_call_can_fail_variants {
        use super::*;

        /// Passes until a step settles at the gate `k` of `turn`, the gate's terminal showing
        /// `code` and `tail` once placed.
        fn complete_k(
            rig: &mut Rig,
            run: &RunId,
            turn: u64,
            k: u32,
            code: i32,
            tail: &str,
        ) -> GateStep {
            for _ in 0..80 {
                match try_go(rig, run) {
                    Err(
                        RunError::LeaseLost(_)
                        | RunError::Tracker(_)
                        | RunError::Worker(_)
                        | RunError::Send(_),
                    ) => rig.guard = Box::new(AlwaysHeld(1)),
                    Err(error) => panic!("{error:?}"),
                    Ok(GateStep::Running) => {
                        show(rig, run, SessionState::Idle, turn, &[exited(k, code)]);
                        for _ in 0..3 {
                            rig.worker.on_tail(Ok(tail.into()));
                        }
                    }
                    Ok(GateStep::NotReady) if k > 1 => {}
                    Ok(step) => return step,
                }
            }
            panic!("the pass did not settle");
        }

        /// Attempt 1 fails and is sent back, attempt 2 ends `code` with `tail`.
        fn two_attempts(
            cut: Option<usize>,
            retries: u64,
            code: i32,
            tail: &str,
        ) -> (GateStep, Vec<Call>, Vec<String>, Vec<String>) {
            let (mut rig, run, hub) = built();
            rig.config.limits.gate_retries = retries;
            // The first create answers the first gate's workspace, the second the second's.
            rig.worker.on_create(Ok(gate_ws(1)));
            rig.worker.on_create(Ok(gate_ws(2)));
            if let Some(checks) = cut {
                rig.guard = Box::new(HeldFor::new(checks));
            }
            assert_eq!(
                complete_k(&mut rig, &run, 1, 1, 101, FAIL_TAIL),
                GateStep::SentBack { attempt: 1 }
            );
            reported_again(&mut rig, &run, &hub, 2);
            let step = complete_k(&mut rig, &run, 2, 2, code, tail);
            (step, calls(&rig), rig.sim.comments(), rig.sim.labels())
        }

        fn same_for_every_cut(retries: u64, code: i32, tail: &str) {
            let baseline = two_attempts(None, retries, code, tail);
            for checks in 1..=40 {
                assert_eq!(
                    two_attempts(Some(checks), retries, code, tail),
                    baseline,
                    "{checks}"
                );
            }
        }

        #[test]
        fn the_pass_path_with_provision_survives_a_failure_of_each_call() {
            let cut = |checks: Option<usize>| {
                let (mut rig, run, _hub) = built();
                rig.config.provision = vec!["npm ci".into()];
                for _ in 0..3 {
                    rig.worker.on_create(Ok(gate_ws(1)));
                }
                if let Some(checks) = checks {
                    rig.guard = Box::new(HeldFor::new(checks));
                }
                let step = {
                    let mut out = GateStep::NotReady;
                    for _ in 0..40 {
                        match try_go(&rig, &run) {
                            Err(_) => rig.guard = Box::new(AlwaysHeld(1)),
                            Ok(GateStep::Running) => {
                                show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 0)]);
                                for _ in 0..3 {
                                    rig.worker
                                        .on_tail(Ok("$ npm ci\n$ cargo test\nok\n".into()));
                                }
                            }
                            Ok(step) => {
                                out = step;
                                break;
                            }
                        }
                    }
                    out
                };
                (step, calls(&rig), rig.sim.comments(), rig.sim.labels())
            };
            let baseline = cut(None);
            assert_eq!(baseline.0, GateStep::Passed { pr: 12 });
            for checks in 1..=30 {
                assert_eq!(cut(Some(checks)), baseline, "{checks}");
            }
        }

        #[test]
        fn the_failed_provision_path_survives_a_failure_of_each_call() {
            let cut = |checks: Option<usize>| {
                let (mut rig, run, _hub) = built();
                rig.config.provision = vec!["npm ci".into()];
                for _ in 0..3 {
                    rig.worker.on_create(Ok(gate_ws(1)));
                }
                if let Some(checks) = checks {
                    rig.guard = Box::new(HeldFor::new(checks));
                }
                complete_k(
                    &mut rig,
                    &run,
                    1,
                    1,
                    1,
                    "$ npm ci\nnpm ERR!\nsetup failed: npm ci exited 1\n",
                );
                (calls(&rig), rig.sim.comments(), rig.sim.labels())
            };
            let baseline = cut(None);
            assert_eq!(sends_of(&baseline.0), 1);
            for checks in 1..=30 {
                assert_eq!(cut(Some(checks)), baseline, "{checks}");
            }
        }

        fn sends_of(calls: &[Call]) -> usize {
            calls
                .iter()
                .filter(|call| matches!(call, Call::Send { .. }))
                .count()
        }

        #[test]
        fn a_pass_on_the_second_attempt_survives_a_failure_of_each_call() {
            same_for_every_cut(2, 0, PASS_TAIL);
        }

        #[test]
        fn the_exhausted_path_after_a_sent_back_attempt_survives_a_failure_of_each_call() {
            same_for_every_cut(1, 101, FAIL_TAIL);
        }
    }

    // ----- review cycle 2 -----

    mod review2_nothing_is_finished_on_a_head_that_was_not_gated {
        use super::*;

        /// The gate passed on HEAD with the pull request out of draft (a person's mark, or this
        /// pass's own that the answer was lost for), and the finish was cut short.
        fn recorded_and_cut(required: bool, by_someone_else: bool) -> (Rig, RunId, Arc<Hub>) {
            let (mut rig, run, hub) = built();
            if required {
                rig.config.required_checks = vec!["ci".into()];
                hub.set_checks(HEAD, &[("ci", "completed", Some("success"))]);
            }
            place(&mut rig, &run, 1, 1);
            if by_someone_else {
                hub.set_draft(false);
            } else {
                rig.sim.lose("pr ready", 1);
            }
            // The label write is what is cut.
            rig.sim.fail("--add-label factory:pr-open", 1);
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 0)]);
            rig.worker.on_tail(Ok(PASS_TAIL.into()));
            assert!(matches!(try_go(&rig, &run), Err(RunError::Tracker(_))));
            assert!(has_step(&rig, &run, "gate-passed "));
            assert!(!hub.lock().pulls[&12].draft);
            (rig, run, hub)
        }

        fn moved_and_gated_again(rig: &mut Rig, run: &RunId, hub: &Hub) {
            hub.set_head(MOVED);
            assert_eq!(go(rig, run), GateStep::NotReady);
            assert!(has_step(rig, run, GATE_HEAD_MOVED));
            assert_eq!(rig.sim.labels(), ["factory:building"]);
            assert!(hub.lock().pulls[&12].draft, "put back to draft");
            rig.worker.on_create(Ok(gate_ws(2)));
            assert_eq!(go(rig, run), GateStep::Running);
            assert!(has_step(rig, run, &format!("{GATE_REQUESTED}2 2 {MOVED}")));
        }

        #[test]
        fn a_pass_the_pr_was_marked_ready_for_by_the_pass_is_gated_again_when_the_head_moved() {
            let (mut rig, run, hub) = recorded_and_cut(false, false);
            moved_and_gated_again(&mut rig, &run, &hub);
        }

        #[test]
        fn a_pass_a_person_marked_ready_is_gated_again_when_the_head_moved() {
            let (mut rig, run, hub) = recorded_and_cut(false, true);
            moved_and_gated_again(&mut rig, &run, &hub);
        }

        #[test]
        fn the_same_with_required_checks_already_read_as_passed_by_the_pass() {
            let (mut rig, run, hub) = recorded_and_cut(true, false);
            moved_and_gated_again(&mut rig, &run, &hub);
        }

        #[test]
        fn the_same_with_required_checks_already_read_as_passed_by_a_person() {
            let (mut rig, run, hub) = recorded_and_cut(true, true);
            moved_and_gated_again(&mut rig, &run, &hub);
        }

        #[test]
        fn a_moved_head_with_no_retry_left_is_stuck_with_the_pull_request_back_in_draft() {
            let (mut rig, run, hub) = recorded_and_cut(false, true);
            rig.config.limits.gate_retries = 0;
            hub.set_head(MOVED);
            let step = go(&rig, &run);
            assert_stuck(&rig, &run, &step, "head-kept-moving", HEAD_KEPT_MOVING);
            assert!(hub.lock().pulls[&12].draft);
        }
    }

    mod review2_the_build_session_is_ended_after_the_ready_mark {
        use super::*;

        #[test]
        fn a_ready_mark_that_is_refused_leaves_the_session_alive_for_a_moved_head_and_a_failure() {
            let (mut rig, run, hub) = built();
            place(&mut rig, &run, 1, 1);
            rig.sim.fail("pr ready", 1);
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 0)]);
            rig.worker.on_tail(Ok(PASS_TAIL.into()));
            assert!(matches!(try_go(&rig, &run), Err(RunError::Tracker(_))));
            assert!(hub.lock().pulls[&12].draft);
            assert!(!calls(&rig).contains(&end_of_build()), "not ended yet");
            hub.set_head(MOVED);
            assert_eq!(go(&rig, &run), GateStep::NotReady);
            place(&mut rig, &run, 1, 2);
            assert_eq!(
                ends(&mut rig, &run, 1, 2, 101, FAIL_TAIL),
                GateStep::SentBack { attempt: 2 }
            );
            assert_eq!(sends(&rig).len(), 1);
            assert!(!calls(&rig).contains(&end_of_build()));
        }

        #[test]
        fn a_failure_with_the_session_already_ended_is_stuck_and_not_sent() {
            let (mut rig, run, hub) = built();
            place(&mut rig, &run, 1, 1);
            // The mark and the end of the session went through; the label did not.
            rig.sim.fail("--add-label factory:pr-open", 1);
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 0)]);
            rig.worker.on_tail(Ok(PASS_TAIL.into()));
            assert!(matches!(try_go(&rig, &run), Err(RunError::Tracker(_))));
            assert!(calls(&rig).contains(&end_of_build()));
            hub.set_head(MOVED);
            assert_eq!(go(&rig, &run), GateStep::NotReady);
            place(&mut rig, &run, 1, 2);
            let step = ends(&mut rig, &run, 1, 2, 101, FAIL_TAIL);
            assert_stuck(&rig, &run, &step, "session-ended", SESSION_ENDED);
            assert_eq!(sends(&rig), vec![]);
        }
    }

    mod review2_an_archive_never_wedges_a_run_and_is_never_faked {
        use super::*;

        fn refuse_archives(rig: &Rig, times: usize) {
            for _ in 0..times {
                rig.worker
                    .on_archive(Err(WorkerError::Failed("the worktree is busy".into())));
            }
        }

        #[test]
        fn a_stop_still_goes_stuck_while_the_gate_workspace_cannot_be_archived() {
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            rig.write(&run, Event::StopRequested { by: "tom".into() });
            refuse_archives(&rig, 10);
            let step = go(&rig, &run);
            assert!(matches!(step, GateStep::Stuck { .. }));
            assert!(rig.finished(&run));
            assert!(!has_step(&rig, &run, &format!("{GATE_ARCHIVED}1")));
        }

        #[test]
        fn a_closed_issue_still_abandons_while_the_gate_workspace_cannot_be_archived() {
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            rig.sim.set_closed(true);
            refuse_archives(&rig, 10);
            assert_eq!(go(&rig, &run), GateStep::Abandoned);
            assert!(rig.finished(&run));
            assert!(!has_step(&rig, &run, &format!("{GATE_ARCHIVED}1")));
        }

        #[test]
        fn an_archive_answered_as_not_a_live_workspace_counts_as_archived() {
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 101)]);
            rig.worker.on_tail(Ok(FAIL_TAIL.into()));
            rig.worker.on_archive(Err(WorkerError::Failed(format!(
                "workspace {} is not a live workspace of this machine",
                gate_ws(1).0
            ))));
            assert_eq!(go(&rig, &run), GateStep::SentBack { attempt: 1 });
            assert!(has_step(&rig, &run, &format!("{GATE_ARCHIVED}1")));
        }

        #[test]
        fn a_recorded_gate_the_view_does_not_show_is_still_archived_by_call() {
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            show(&mut rig, &run, SessionState::Idle, 1, &[]);
            rig.write(&run, Event::StopRequested { by: "tom".into() });
            assert!(matches!(go(&rig, &run), GateStep::Stuck { .. }));
            assert_eq!(archived(&rig), vec![gate_ws(1)]);
            assert!(has_step(&rig, &run, &format!("{GATE_ARCHIVED}1")));
        }

        #[test]
        fn three_refused_archives_leave_the_gate_recorded_and_the_run_carries_on() {
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            show(
                &mut rig,
                &run,
                SessionState::Idle,
                1,
                &[G {
                    k: 1,
                    state: TerminalState::Exited,
                    code: None,
                }],
            );
            refuse_archives(&rig, 10);
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 1 });
            assert!(matches!(try_go(&rig, &run), Err(RunError::Worker(_))));
            assert!(matches!(try_go(&rig, &run), Err(RunError::Worker(_))));
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 0 });
            assert!(!rig.finished(&run));
            assert_eq!(step_count(&rig, &run, GATE_LEFT), 1);
            assert!(has_step(
                &rig,
                &run,
                &format!("{GATE_LEFT}1 {} ", gate_ws(1).0)
            ));
            assert!(!has_step(&rig, &run, &format!("{GATE_ARCHIVED}1")));
            assert!(
                rig.steps(&run)
                    .iter()
                    .filter(|step| step.starts_with(GATE_LEFT))
                    .all(|step| !step.contains("busy"))
            );
            // The next gate is placed and the run goes on.
            show(&mut rig, &run, SessionState::Idle, 1, &[]);
            rig.worker.on_create(Ok(gate_ws(2)));
            assert_eq!(go(&rig, &run), GateStep::Running);
        }

        #[test]
        fn an_exit_path_whose_archive_is_refused_or_unreachable_records_the_gate_left_once() {
            for error in [
                WorkerError::Failed("the worktree is busy".into()),
                WorkerError::Unreachable("no route".into()),
            ] {
                let (mut rig, run, _hub) = built();
                place(&mut rig, &run, 1, 1);
                rig.write(&run, Event::StopRequested { by: "tom".into() });
                rig.worker.on_archive(Err(error));
                assert!(matches!(go(&rig, &run), GateStep::Stuck { .. }));
                assert_eq!(step_count(&rig, &run, GATE_LEFT), 1);
                assert!(!has_step(&rig, &run, &format!("{GATE_ARCHIVED}1")));
                assert!(!gate_running(&rig.events(&run)));
            }
        }

        #[test]
        fn an_archive_refused_as_unreachable_by_a_machine_that_is_online_is_asked_again() {
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 101)]);
            rig.worker.on_tail(Ok(FAIL_TAIL.into()));
            rig.worker
                .on_archive(Err(WorkerError::Unreachable("no route".into())));
            // Not an error: the verdict waits for the archive.
            assert_eq!(go(&rig, &run), GateStep::NotReady);
            assert!(!has_step(&rig, &run, &format!("{GATE_ARCHIVED}1")));
            assert_eq!(go(&rig, &run), GateStep::SentBack { attempt: 1 });
            assert!(has_step(&rig, &run, &format!("{GATE_ARCHIVED}1")));
            assert_eq!(archived(&rig), vec![gate_ws(1), gate_ws(1)]);
        }

        #[test]
        fn a_gate_of_a_machine_that_is_gone_is_left_to_the_sweep_and_blocks_nothing() {
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            show_gone(
                &mut rig,
                &run,
                1,
                &[running(1)],
                Duration::from_secs(6 * 60),
            );
            rig.write(&run, Event::StopRequested { by: "tom".into() });
            let step = go(&rig, &run);
            assert!(matches!(step, GateStep::Stuck { .. }));
            assert_eq!(archived(&rig), vec![]);
            assert_eq!(step_count(&rig, &run, GATE_LEFT), 1);
        }

        #[test]
        fn a_gate_is_never_found_or_archived_by_asking_for_it_to_be_made_again() {
            // After a create that got no answer, on the placement path and on an exit path.
            let (rig, run, _hub) = built();
            rig.worker.on_create(Err(WorkerError::Unreachable(
                "did not answer in time".into(),
            )));
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 1 });
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 0 });
            assert_eq!(creates(&rig), 1);
            rig.write(&run, Event::StopRequested { by: "tom".into() });
            assert!(matches!(go(&rig, &run), GateStep::Stuck { .. }));
            assert_eq!(creates(&rig), 1);
            assert_eq!(archived(&rig), vec![]);
            assert_eq!(step_count(&rig, &run, GATE_LEFT), 1);
        }

        #[test]
        fn a_create_that_got_no_answer_whose_workspace_later_appears_by_name_is_archived() {
            let (mut rig, run, _hub) = built();
            rig.worker.on_create(Err(WorkerError::Unreachable(
                "did not answer in time".into(),
            )));
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 1 });
            show(&mut rig, &run, SessionState::Idle, 1, &[running(1)]);
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 0 });
            assert_eq!(archived(&rig), vec![gate_ws(1)]);
            assert_eq!(creates(&rig), 1);
        }

        #[test]
        fn a_create_the_worker_refused_that_never_appears_is_recorded_left_and_not_archived() {
            let (mut rig, run, _hub) = built();
            rig.worker
                .on_create(Err(WorkerError::Failed("no disk".into())));
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 1 });
            // A refusal made nothing: it is left at once, with no grace.
            assert_eq!(step_count(&rig, &run, &format!("{GATE_LEFT}1 - ")), 1);
            rig.worker.on_create(Ok(gate_ws(2)));
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 0 });
            assert_eq!(go(&rig, &run), GateStep::Running);
            show(&mut rig, &run, SessionState::Idle, 1, &[running(2)]);
            assert_eq!(
                ends(&mut rig, &run, 1, 2, 0, PASS_TAIL),
                GateStep::Passed { pr: 12 }
            );
            assert_eq!(step_count(&rig, &run, &format!("{GATE_LEFT}1 - ")), 1);
            assert!(!has_step(&rig, &run, &format!("{GATE_ARCHIVED}1")));
            assert_eq!(creates(&rig), 2);
            assert!(!archived(&rig).contains(&gate_ws(1)));
        }

        #[test]
        fn a_refused_create_is_recorded_could_not_run_before_it_is_recorded_left() {
            // A pass cut between the two steps must leave a gate that could not run, which is
            // archived or left before another is placed, and never a gate left that is not
            // could-not-run, which a later pass would read as in flight and ask for again.
            for refusals in [1, 5] {
                let (rig, run, _hub) = built();
                for _ in 0..refusals {
                    rig.worker
                        .on_create(Err(WorkerError::Failed("no disk".into())));
                }
                for _ in 0..30 {
                    if matches!(go(&rig, &run), GateStep::Stuck { .. }) || creates(&rig) == refusals
                    {
                        break;
                    }
                }
                let steps = rig.steps(&run);
                let at = |prefix: &str| {
                    steps
                        .iter()
                        .rposition(|step| step.starts_with(prefix))
                        .unwrap_or_else(|| panic!("no {prefix:?} in {steps:?}"))
                };
                let last = format!("{GATE_LEFT}{refusals} - {LEFT_CREATE_REFUSED}");
                assert!(at(GATE_COULD_NOT_RUN) < at(&last), "{steps:?}");
            }
        }

        #[test]
        fn a_person_marking_the_pr_ready_while_the_gate_ran_and_a_moved_head_gates_again() {
            let (mut rig, run, hub) = built();
            place(&mut rig, &run, 1, 1);
            hub.set_draft(false);
            hub.set_head(MOVED);
            assert_eq!(ends(&mut rig, &run, 1, 1, 0, PASS_TAIL), GateStep::NotReady);
            assert!(
                hub.lock().pulls[&12].draft,
                "put back to draft by the settle path"
            );
            assert_eq!(rig.sim.labels(), ["factory:building"]);
            assert!(!has_step(&rig, &run, "gate-passed "));
            rig.worker.on_create(Ok(gate_ws(2)));
            assert_eq!(go(&rig, &run), GateStep::Running);
            assert!(has_step(
                &rig,
                &run,
                &format!("{GATE_REQUESTED}2 2 {MOVED}")
            ));
        }
    }

    mod review2_what_was_recorded_is_written_once_and_in_full {
        use super::*;

        #[test]
        fn a_conflict_attempt_cut_before_its_send_then_gated_records_its_own_commands() {
            let (mut rig, run, hub) = built();
            hub.set_mergeable("CONFLICTING");
            // The conflict line is written, then reading the config for the note fails.
            hub.fail("api repos/rawphp/softfac", 1);
            assert!(matches!(try_go(&rig, &run), Err(RunError::Tracker(_))));
            assert_eq!(event_count(&rig, &run, "gate_command"), 1);
            // The conflict clears; the same attempt is gated and passes.
            hub.set_mergeable("MERGEABLE");
            assert_eq!(
                gated(&mut rig, &run, 0, PASS_TAIL),
                GateStep::Passed { pr: 12 }
            );
            let comment = rig
                .sim
                .comments()
                .into_iter()
                .find(|comment| comment.contains("Factory gate: passed"))
                .expect("the results");
            assert!(comment.contains("`cargo test` | 0"), "{comment}");
            assert!(!comment.contains("`merge`"), "{comment}");
        }

        #[test]
        fn command_lines_cut_part_way_are_completed_for_the_gate() {
            let (mut rig, run, _hub) = built();
            rig.config.provision = vec!["npm ci".into()];
            rig.config.gate = vec!["cargo fmt".into(), "cargo test".into()];
            place(&mut rig, &run, 1, 1);
            rig.write(
                &run,
                Event::GateCommand {
                    attempt: 1,
                    command: "npm ci".into(),
                    exit_code: 0,
                    duration_ms: 0,
                },
            );
            assert_eq!(
                ends(&mut rig, &run, 1, 1, 0, PASS_TAIL),
                GateStep::Passed { pr: 12 }
            );
            let comment = rig
                .sim
                .comments()
                .into_iter()
                .find(|comment| comment.contains("Factory gate: passed"))
                .expect("the results");
            for command in ["npm ci", "cargo fmt", "cargo test"] {
                assert_eq!(
                    comment.matches(&format!("`{command}`")).count(),
                    1,
                    "{comment}"
                );
            }
        }

        #[test]
        fn a_failure_owed_to_a_session_whose_process_ended_is_not_sent_again_and_again() {
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            show(&mut rig, &run, SessionState::Finished, 1, &[exited(1, 101)]);
            rig.worker.on_tail(Ok(FAIL_TAIL.into()));
            assert_eq!(go(&rig, &run), GateStep::NotReady);
            assert_eq!(go(&rig, &run), GateStep::NotReady);
            assert_eq!(sends(&rig), vec![]);
            assert_eq!(step_count(&rig, &run, "send-intent"), 0);
        }
    }

    mod review2_the_guards_are_reached {
        use super::*;

        #[test]
        fn a_reported_pull_request_whose_session_has_no_turn_record_is_not_gated() {
            let (mut rig, run, hub) = built();
            assert_eq!(
                gated(&mut rig, &run, 101, FAIL_TAIL),
                GateStep::SentBack { attempt: 1 }
            );
            reported_again(&mut rig, &run, &hub, 2);
            show(&mut rig, &run, SessionState::Idle, 0, &[]);
            assert_eq!(go(&rig, &run), GateStep::NotReady);
            assert_eq!(creates(&rig), 1);
        }

        #[test]
        fn a_send_owed_to_a_session_with_no_turn_record_waits() {
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            rig.worker.on_send(Err(SendRefused::Failed {
                reason: "the answer was lost".into(),
                prompts: Some(1),
            }));
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 101)]);
            rig.worker.on_tail(Ok(FAIL_TAIL.into()));
            assert!(matches!(try_go(&rig, &run), Err(RunError::Send(_))));
            show(&mut rig, &run, SessionState::Idle, 0, &[]);
            assert_eq!(go(&rig, &run), GateStep::NotReady);
            assert_eq!(sends(&rig).len(), 1);
        }

        #[test]
        fn a_branch_a_launch_was_resumed_from_is_bound_at_gate_time() {
            let (mut rig, run, hub) = built();
            rig.step(&run, "launch-sending resumed-from fac/9-1");
            hub.edit(12, |pull| pull.head_ref = "fac/9-1".into());
            assert_eq!(
                gated(&mut rig, &run, 0, PASS_TAIL),
                GateStep::Passed { pr: 12 }
            );
        }

        #[test]
        fn a_second_rounds_budget_counts_from_the_rounds_first_attempt() {
            let (mut rig, run, hub) = built();
            rig.config.limits.gate_retries = 1;
            second_round(&mut rig, &run, &hub);
            place(&mut rig, &run, 2, 2);
            // The round's first failure goes back: under a count from the run's first attempt
            // it would already be the second.
            assert_eq!(
                ends(&mut rig, &run, 2, 2, 101, FAIL_TAIL),
                GateStep::SentBack { attempt: 2 }
            );
            reported_again(&mut rig, &run, &hub, 3);
            place(&mut rig, &run, 3, 3);
            let step = ends(&mut rig, &run, 3, 3, 101, FAIL_TAIL);
            let GateStep::Stuck { .. } = &step else {
                panic!("expected stuck, got {step:?}");
            };
            assert!(
                rig.sim.comments().iter().any(|comment| comment
                    .contains("Reason: the gate failed after 2 attempts: `cargo test` failed")),
                "{:?}",
                rig.sim.comments()
            );
        }
    }

    mod review4_an_archive_is_only_what_a_call_answered {
        use super::*;

        /// Three refusals leave g1 recorded left; the run goes on and g2 passes.
        pub(super) fn g1_left_then_g2_ready_to_finish() -> (Rig, RunId, Arc<Hub>) {
            let (mut rig, run, hub) = built();
            place(&mut rig, &run, 1, 1);
            show(
                &mut rig,
                &run,
                SessionState::Idle,
                1,
                &[G {
                    k: 1,
                    state: TerminalState::Exited,
                    code: None,
                }],
            );
            for _ in 0..3 {
                rig.worker
                    .on_archive(Err(WorkerError::Failed("the worktree is busy".into())));
            }
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 1 });
            let _ = try_go(&rig, &run);
            let _ = try_go(&rig, &run);
            assert_eq!(step_count(&rig, &run, GATE_LEFT), 0);
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 0 });
            assert_eq!(step_count(&rig, &run, GATE_LEFT), 1);
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 0)]);
            place(&mut rig, &run, 1, 2);
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(2, 0)]);
            rig.worker.on_tail(Ok(PASS_TAIL.into()));
            (rig, run, hub)
        }

        #[test]
        fn a_gate_with_no_workspace_to_archive_blocks_the_next_until_it_is_left() {
            let (rig, run, _hub) = built();
            rig.worker
                .on_create(Err(WorkerError::Unreachable("no answer".into())));
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 1 });
            rig.worker.on_create(Ok(gate_ws(2)));
            for pass in 1..=10 {
                let step = go(&rig, &run);
                if step == GateStep::Running {
                    assert_eq!(step_count(&rig, &run, GATE_LEFT), 1, "pass {pass}");
                    return;
                }
                assert_eq!(creates(&rig), 1, "pass {pass}: nothing is placed yet");
            }
            panic!("the second gate was never placed");
        }

        #[test]
        fn a_gate_left_on_an_online_machine_is_archived_again_before_pr_open() {
            let (rig, run, _hub) = g1_left_then_g2_ready_to_finish();
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
            assert!(archived(&rig).contains(&gate_ws(1)));
            assert!(has_step(&rig, &run, &format!("{GATE_ARCHIVED}1")));
            assert_eq!(rig.sim.labels(), ["factory:pr-open"]);
        }

        #[test]
        fn a_gate_that_is_still_refused_at_pr_open_holds_it_back_and_ends_the_run_stuck() {
            let (rig, run, hub) = g1_left_then_g2_ready_to_finish();
            rig.worker.on_archive(Ok(())); // g2's own
            rig.worker
                .on_archive(Err(WorkerError::Failed("the worktree is busy".into())));
            let step = go(&rig, &run);
            assert!(hub.lock().pulls[&12].draft, "no ready mark");
            let GateStep::Stuck { reason } = &step else {
                panic!("expected stuck, got {step:?}");
            };
            assert!(
                reason.contains("a gate workspace could not be archived"),
                "{reason}"
            );
            assert!(
                !rig.events(&run)
                    .iter()
                    .any(|event| event.event == "state_changed" && event.data["to"] == "pr-open")
            );
        }

        #[test]
        fn a_gate_left_because_its_machine_is_gone_does_not_hold_pr_open() {
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            show_gone(
                &mut rig,
                &run,
                1,
                &[running(1)],
                Duration::from_secs(6 * 60),
            );
            assert!(matches!(go(&rig, &run), GateStep::BackingOff { .. }));
            assert!(matches!(go(&rig, &run), GateStep::BackingOff { .. }));
            assert_eq!(step_count(&rig, &run, GATE_LEFT), 1);
            show(&mut rig, &run, SessionState::Idle, 1, &[]);
            let mut passes = 0;
            rig.worker.on_create(Ok(gate_ws(2)));
            while go(&rig, &run) != GateStep::Running {
                passes += 1;
                assert!(passes < 10);
            }
            show(&mut rig, &run, SessionState::Idle, 1, &[running(2)]);
            assert_eq!(
                ends(&mut rig, &run, 1, 2, 0, PASS_TAIL),
                GateStep::Passed { pr: 12 }
            );
            // Its machine answers again by now: decided from the view now, not from why it was
            // left, it is archived by call.
            assert!(archived(&rig).contains(&gate_ws(1)));
        }

        #[test]
        fn a_pass_cut_after_the_label_sweeps_the_gates_of_the_run_on_the_next_call() {
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            // The run is already pr-open, its gate never archived.
            rig.moved(&run, State::Building, State::PrOpen);
            rig.step(&run, BUILD_SESSION_ENDED);
            rig.step(&run, BUILD_ARCHIVED);
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
            assert_eq!(archived(&rig), vec![gate_ws(1)]);
        }

        #[test]
        fn the_build_workspace_answered_as_not_live_counts_as_archived() {
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 0)]);
            rig.worker.on_tail(Ok(PASS_TAIL.into()));
            rig.worker.on_archive(Ok(())); // the gate's
            rig.worker.on_archive(Err(WorkerError::Failed(format!(
                "workspace {} is not a live workspace of this machine",
                workspace().0
            ))));
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
            assert!(has_step(&rig, &run, BUILD_ARCHIVED));
        }

        #[test]
        fn an_unreachable_answer_from_a_machine_that_is_not_gone_does_not_count_toward_the_tries() {
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            show(
                &mut rig,
                &run,
                SessionState::Idle,
                1,
                &[G {
                    k: 1,
                    state: TerminalState::Exited,
                    code: None,
                }],
            );
            for _ in 0..5 {
                rig.worker
                    .on_archive(Err(WorkerError::Unreachable("no route".into())));
            }
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 1 });
            for _ in 0..5 {
                let step = go(&rig, &run);
                assert!(matches!(step, GateStep::BackingOff { .. }), "{step:?}");
            }
            assert_eq!(step_count(&rig, &run, GATE_LEFT), 0);
            assert_eq!(
                creates(&rig),
                1,
                "nothing is placed while the archive is unanswered"
            );
            // The archive answers: the gate is archived, not left, and the next is placed.
            rig.worker.on_create(Ok(gate_ws(2)));
            assert_eq!(go(&rig, &run), GateStep::Running);
            assert_eq!(step_count(&rig, &run, GATE_LEFT), 0);
            assert!(has_step(&rig, &run, &format!("{GATE_ARCHIVED}1")));
        }
    }

    mod review5 {
        use super::*;

        #[test]
        fn a_build_workspace_answered_unreachable_is_asked_again_while_its_machine_is_not_gone() {
            let (mut rig, run, _hub) = built();
            rig.moved(&run, State::Building, State::PrOpen);
            rig.step(&run, BUILD_SESSION_ENDED);
            show_gone(&mut rig, &run, 1, &[], Duration::from_secs(6 * 60));
            rig.worker
                .on_archive(Err(WorkerError::Unreachable("no route".into())));
            // Not an error: the run stays as it is and the archive is asked again.
            assert_eq!(try_go(&rig, &run).unwrap(), GateStep::NotReady);
            assert!(
                !rig.steps(&run)
                    .iter()
                    .any(|step| step.contains("build-left"))
            );
            assert_eq!(archived(&rig), vec![workspace()]);
            show(&mut rig, &run, SessionState::Idle, 1, &[]);
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
            assert_eq!(archived(&rig), vec![workspace(), workspace()]);
            assert!(has_step(&rig, &run, BUILD_ARCHIVED));
        }

        #[test]
        fn a_gate_left_unshown_and_shown_live_later_is_archived_before_pr_open() {
            let (mut rig, run, _hub) = built();
            rig.worker
                .on_create(Err(WorkerError::Unreachable("no answer".into())));
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 1 });
            rig.worker.on_create(Ok(gate_ws(2)));
            let mut passes = 0;
            while go(&rig, &run) != GateStep::Running {
                passes += 1;
                assert!(passes < 20);
            }
            assert_eq!(step_count(&rig, &run, GATE_LEFT), 1);
            show(
                &mut rig,
                &run,
                SessionState::Idle,
                1,
                &[running(1), exited(2, 0)],
            );
            rig.worker.on_tail(Ok(PASS_TAIL.into()));
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
            assert!(archived(&rig).contains(&gate_ws(1)));
        }

        #[test]
        fn a_stuck_sweep_on_the_merged_path_is_returned_and_finishes_the_run_once() {
            let (mut rig, run, hub) = built();
            place(&mut rig, &run, 1, 1);
            show(
                &mut rig,
                &run,
                SessionState::Idle,
                1,
                &[G {
                    k: 1,
                    state: TerminalState::Exited,
                    code: None,
                }],
            );
            for _ in 0..3 {
                rig.worker
                    .on_archive(Err(WorkerError::Failed("the worktree is busy".into())));
            }
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 1 });
            let _ = try_go(&rig, &run);
            let _ = try_go(&rig, &run);
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 0 });
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 0)]);
            place(&mut rig, &run, 1, 2);
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(2, 0)]);
            rig.worker.on_tail(Ok(PASS_TAIL.into()));
            hub.set_state("MERGED");
            rig.worker.on_archive(Ok(()));
            rig.worker
                .on_archive(Err(WorkerError::Failed("the worktree is busy".into())));
            let step = go(&rig, &run);
            assert!(matches!(step, GateStep::Stuck { .. }), "{step:?}");
            assert_eq!(event_count(&rig, &run, "finished"), 1);
        }

        #[test]
        fn an_unreachable_archive_from_an_online_machine_waits_5_minutes_on_the_clock_then_is_left()
        {
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 101)]);
            rig.worker.on_tail(Ok(FAIL_TAIL.into()));
            for _ in 0..4 {
                rig.worker
                    .on_archive(Err(WorkerError::Unreachable("no route".into())));
            }
            // Not an error: the verdict waits for the archive, the wait started once.
            for _ in 0..3 {
                assert_eq!(try_go(&rig, &run).unwrap(), GateStep::NotReady);
                rig.clock.advance(Duration::from_secs(60));
            }
            assert_eq!(step_count(&rig, &run, "gate-archive-waiting 1 "), 1);
            assert_eq!(sends(&rig), vec![]);
            rig.clock.advance(Duration::from_secs(2 * 60));
            assert_eq!(go(&rig, &run), GateStep::SentBack { attempt: 1 });
            assert_eq!(
                step_count(
                    &rig,
                    &run,
                    &format!("{GATE_LEFT}1 {} {LEFT_UNREACHABLE}", gate_ws(1).0)
                ),
                1
            );
            assert!(!has_step(&rig, &run, &format!("{GATE_ARCHIVED}1")));
        }
    }

    mod review6 {
        use super::*;

        #[test]
        fn three_refused_archives_of_the_build_workspace_at_pr_open_end_the_run_stuck() {
            let (mut rig, run, hub) = built();
            rig.moved(&run, State::Building, State::PrOpen);
            rig.sim.set_labels(&["factory:pr-open"]);
            hub.set_draft(false);
            rig.step(&run, BUILD_SESSION_ENDED);
            show(&mut rig, &run, SessionState::Idle, 1, &[]);
            for _ in 0..3 {
                rig.worker
                    .on_archive(Err(WorkerError::Failed("the worktree is busy".into())));
            }
            assert!(matches!(try_go(&rig, &run), Err(RunError::Worker(_))));
            assert!(matches!(try_go(&rig, &run), Err(RunError::Worker(_))));
            let step = go(&rig, &run);
            let GateStep::Stuck { reason } = &step else {
                panic!("expected stuck, got {step:?}");
            };
            // The words name the workspace that could not be archived: the build's.
            assert!(
                reason.contains("the build workspace could not be archived"),
                "{reason}"
            );
            assert!(!reason.contains("gate workspace"), "{reason}");
            // The pull request was marked ready before the label moved: the stuck run puts it
            // back to draft, so nothing is left looking ready.
            assert!(
                hub.lock().pulls[&12].draft,
                "the pull request is a draft again"
            );
            assert_eq!(rig.sim.labels(), ["factory:stuck"]);
        }

        #[test]
        fn the_unseen_wait_says_the_passes_it_has_left() {
            let (rig, run, _hub) = built();
            rig.worker
                .on_create(Err(WorkerError::Unreachable("no answer".into())));
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 1 });
            // The backoff's own pass.
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 0 });
            // Then the grace for the workspace to be shown, on top of the backoff.
            for left in [3, 2, 1, 0] {
                assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: left });
            }
            assert_eq!(go(&rig, &run), GateStep::Running);
        }
    }

    mod leftovers_of_cycle_7 {
        use super::*;

        /// A run in `pr-open` whose build session was ended, with the build workspace still to
        /// archive.
        fn at_pr_open() -> (Rig, RunId, Arc<Hub>) {
            let (mut rig, run, hub) = built();
            rig.moved(&run, State::Building, State::PrOpen);
            rig.sim.set_labels(&["factory:pr-open"]);
            hub.set_draft(false);
            rig.step(&run, BUILD_SESSION_ENDED);
            show(&mut rig, &run, SessionState::Idle, 1, &[]);
            (rig, run, hub)
        }

        #[test]
        fn a_build_workspace_whose_machine_is_gone_does_not_hold_pr_open() {
            let (mut rig, run, _hub) = at_pr_open();
            rig.view = FleetView::builder()
                .session(session_record(SessionState::Idle))
                .unreachable(builder(), crate::factory::runs::progress::UNREACHABLE_FOR)
                .build();
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
            assert_eq!(archived(&rig), vec![]);
            assert_eq!(
                step_count(&rig, &run, &format!("{BUILD_LEFT}{LEFT_GONE}")),
                1
            );
            assert!(!has_step(&rig, &run, BUILD_ARCHIVED));
            // Nothing is asked again on a later pass.
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
            assert_eq!(archived(&rig), vec![]);
            assert_eq!(step_count(&rig, &run, BUILD_LEFT), 1);
        }

        #[test]
        fn a_build_workspace_whose_machine_is_unreachable_for_less_than_15_minutes_is_asked() {
            let (mut rig, run, _hub) = at_pr_open();
            rig.view = FleetView::builder()
                .session(session_record(SessionState::Idle))
                .unreachable(
                    builder(),
                    crate::factory::runs::progress::UNREACHABLE_FOR - Duration::from_secs(1),
                )
                .build();
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
            assert_eq!(archived(&rig), vec![workspace()]);
            assert!(!has_step(&rig, &run, BUILD_LEFT));
        }

        #[test]
        fn a_build_archive_answered_unreachable_waits_15_minutes_on_the_clock_then_is_left() {
            let (rig, run, hub) = at_pr_open();
            for _ in 0..4 {
                rig.worker
                    .on_archive(Err(WorkerError::Unreachable("no route".into())));
            }
            for _ in 0..3 {
                assert_eq!(try_go(&rig, &run).unwrap(), GateStep::NotReady);
                rig.clock.advance(Duration::from_secs(4 * 60));
            }
            assert_eq!(step_count(&rig, &run, BUILD_ARCHIVE_WAITING), 1);
            assert!(!has_step(&rig, &run, BUILD_LEFT));
            rig.clock.advance(Duration::from_secs(3 * 60));
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
            assert_eq!(
                step_count(&rig, &run, &format!("{BUILD_LEFT}{LEFT_UNREACHABLE}")),
                1
            );
            assert!(!has_step(&rig, &run, BUILD_ARCHIVED));
            assert_eq!(rig.sim.labels(), ["factory:pr-open"]);
            assert!(!hub.lock().pulls[&12].draft);
        }

        #[test]
        fn a_gate_archive_answered_unreachable_holds_pr_open_5_minutes_then_does_not() {
            let (rig, run, hub) = super::review4_an_archive_is_only_what_a_call_answered::g1_left_then_g2_ready_to_finish();
            rig.worker.on_archive(Ok(())); // g2's own
            for _ in 0..3 {
                rig.worker
                    .on_archive(Err(WorkerError::Unreachable("no route".into())));
            }
            assert_eq!(try_go(&rig, &run).unwrap(), GateStep::NotReady);
            assert!(
                hub.lock().pulls[&12].draft,
                "no ready mark while g1 may be live"
            );
            rig.clock.advance(Duration::from_secs(4 * 60));
            assert_eq!(try_go(&rig, &run).unwrap(), GateStep::NotReady);
            assert_eq!(step_count(&rig, &run, "gate-archive-waiting 1 "), 1);
            rig.clock.advance(Duration::from_secs(60));
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
            assert_eq!(rig.sim.labels(), ["factory:pr-open"]);
        }

        #[test]
        fn a_could_not_run_gate_answered_unreachable_backs_off_then_is_left_after_5_minutes() {
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            show(
                &mut rig,
                &run,
                SessionState::Idle,
                1,
                &[G {
                    k: 1,
                    state: TerminalState::Exited,
                    code: None,
                }],
            );
            for _ in 0..20 {
                rig.worker
                    .on_archive(Err(WorkerError::Unreachable("no route".into())));
            }
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 1 });
            for _ in 0..4 {
                let step = try_go(&rig, &run).unwrap();
                assert!(matches!(step, GateStep::BackingOff { .. }), "{step:?}");
                assert_eq!(creates(&rig), 1, "nothing is placed while g1 may be live");
            }
            assert_eq!(step_count(&rig, &run, "gate-archive-waiting 1 "), 1);
            rig.clock.advance(gate::MACHINE_GONE_AFTER);
            rig.worker.on_create(Ok(gate_ws(2)));
            let mut placed = false;
            for _ in 0..10 {
                if go(&rig, &run) == GateStep::Running {
                    placed = true;
                    break;
                }
            }
            assert!(placed, "the second gate is placed");
            assert_eq!(
                step_count(
                    &rig,
                    &run,
                    &format!("{GATE_LEFT}1 {} {LEFT_UNREACHABLE}", gate_ws(1).0)
                ),
                1
            );
        }
    }

    mod round1_of_the_hand_finish {
        use super::*;

        #[test]
        fn a_pull_request_from_a_fork_is_stuck_before_any_gate_is_placed() {
            let (rig, run, hub) = built();
            hub.edit(12, |pull| pull.cross_repository = true);
            let step = go(&rig, &run);
            assert_stuck(
                &rig,
                &run,
                &step,
                "pr-not-bound",
                "the session reported PR #12 but it comes from a fork",
            );
            assert_eq!(creates(&rig), 0);
        }

        #[test]
        fn a_pull_request_that_comes_from_a_fork_when_the_gate_ends_is_not_passed() {
            let (mut rig, run, hub) = built();
            place(&mut rig, &run, 1, 1);
            hub.edit(12, |pull| pull.cross_repository = true);
            let step = ends(&mut rig, &run, 1, 1, 0, PASS_TAIL);
            assert_stuck(
                &rig,
                &run,
                &step,
                "pr-not-bound",
                "the session reported PR #12 but it comes from a fork",
            );
            assert!(!has_step(&rig, &run, "gate-passed "));
            assert!(hub.lock().pulls[&12].draft);
        }

        #[test]
        fn a_merge_while_the_build_archive_waits_leaves_the_run_open_until_it_is_archived() {
            let (mut rig, run, hub) = built();
            place(&mut rig, &run, 1, 1);
            hub.set_state("MERGED");
            rig.worker.on_archive(Ok(())); // the gate's own
            rig.worker
                .on_archive(Err(WorkerError::Unreachable("no route".into())));
            let step = ends(&mut rig, &run, 1, 1, 0, PASS_TAIL);
            assert_eq!(step, GateStep::NotReady);
            assert!(
                !rig.finished(&run),
                "the run is not ended with its workspace live"
            );
            // The archive answers on a later pass, which finishes the recorded pass as the
            // runner's two-step case does: the run is left in `pr-open`, and ending it as merged
            // is the feedback step's (task 28), as for a merge after a recorded pass.
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
            assert!(has_step(&rig, &run, BUILD_ARCHIVED));
            assert_eq!(
                calls(&rig)
                    .iter()
                    .filter(|call| **call == archive_of(workspace()))
                    .count(),
                2
            );
            assert_eq!(rig.sim.labels(), ["factory:pr-open"]);
            assert!(!rig.finished(&run));
        }

        #[test]
        fn a_merge_while_an_earlier_gate_archive_waits_ends_no_session_and_leaves_the_run_open() {
            let (rig, run, hub) =
                super::review4_an_archive_is_only_what_a_call_answered::g1_left_then_g2_ready_to_finish();
            hub.set_state("MERGED");
            rig.worker.on_archive(Ok(())); // g2's own
            rig.worker
                .on_archive(Err(WorkerError::Unreachable("no route".into())));
            assert_eq!(go(&rig, &run), GateStep::NotReady);
            assert!(!rig.finished(&run));
            assert!(!has_step(&rig, &run, BUILD_SESSION_ENDED));
        }

        /// The gate `k` exited with `code` on the online gate machine, and the build machine
        /// the view shows unreachable for `span`.
        fn build_machine_away(rig: &mut Rig, run: &RunId, k: u32, code: i32, span: Duration) {
            let mut view = FleetView::builder()
                .online(machine())
                .session(session_record(SessionState::Idle))
                .turn(SessionTurn {
                    session: session(),
                    workspace: workspace(),
                    project: project_id(),
                    turn: 1,
                    prompts: 1,
                    question: false,
                })
                .unreachable(builder(), span);
            for (workspace, terminal) in gate_records(run, &[exited(k, code)]) {
                view = view.workspace(workspace).terminal(terminal);
            }
            rig.view = view.build();
        }

        #[test]
        fn a_pass_whose_build_machine_is_gone_reaches_pr_open_without_ending_or_archiving() {
            let (mut rig, run, hub) = built();
            place(&mut rig, &run, 1, 1);
            // The pass is recorded and the pull request marked ready; the end of the session
            // gets no answer, and then the build machine is gone.
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 0)]);
            rig.worker.on_tail(Ok(PASS_TAIL.into()));
            rig.worker
                .on_end_sessions(Err(WorkerError::Unreachable("no route".into())));
            assert_eq!(go(&rig, &run), GateStep::NotReady);
            assert!(!hub.lock().pulls[&12].draft);
            build_machine_away(
                &mut rig,
                &run,
                1,
                0,
                crate::factory::runs::progress::UNREACHABLE_FOR,
            );
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
            assert!(!hub.lock().pulls[&12].draft);
            assert_eq!(rig.sim.labels(), ["factory:pr-open"]);
            assert_eq!(
                calls(&rig)
                    .iter()
                    .filter(|call| **call == end_of_build())
                    .count(),
                1,
                "the one end that got no answer"
            );
            assert!(!archived(&rig).contains(&workspace()));
            assert_eq!(
                step_count(&rig, &run, &format!("{BUILD_SESSION_LEFT}{LEFT_GONE}")),
                1
            );
            assert_eq!(
                step_count(&rig, &run, &format!("{BUILD_LEFT}{LEFT_GONE}")),
                1
            );
        }

        #[test]
        fn an_end_of_sessions_answered_unreachable_waits_15_minutes_on_the_clock_then_is_left() {
            let (mut rig, run, hub) = built();
            place(&mut rig, &run, 1, 1);
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 0)]);
            rig.worker.on_tail(Ok(PASS_TAIL.into()));
            for _ in 0..4 {
                rig.worker
                    .on_end_sessions(Err(WorkerError::Unreachable("no route".into())));
            }
            for _ in 0..3 {
                assert_eq!(try_go(&rig, &run).unwrap(), GateStep::NotReady);
                rig.clock.advance(Duration::from_secs(4 * 60));
            }
            // The ready mark came first; the label waits for the session's end.
            assert!(!hub.lock().pulls[&12].draft);
            assert_eq!(rig.sim.labels(), ["factory:building"]);
            assert_eq!(step_count(&rig, &run, BUILD_SESSION_WAITING), 1);
            rig.clock.advance(Duration::from_secs(3 * 60));
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
            assert_eq!(
                step_count(
                    &rig,
                    &run,
                    &format!("{BUILD_SESSION_LEFT}{LEFT_UNREACHABLE}")
                ),
                1
            );
            assert!(!has_step(&rig, &run, BUILD_SESSION_ENDED));
            assert_eq!(rig.sim.labels(), ["factory:pr-open"]);
        }

        #[test]
        fn a_stuck_run_whose_session_end_is_unreachable_is_stuck_at_once() {
            let (rig, run, _hub) = built();
            rig.write(&run, Event::StopRequested { by: "tom".into() });
            rig.worker
                .on_end_sessions(Err(WorkerError::Unreachable("no route".into())));
            let step = go(&rig, &run);
            assert!(matches!(step, GateStep::Stuck { .. }), "{step:?}");
            assert_eq!(
                step_count(
                    &rig,
                    &run,
                    &format!("{BUILD_SESSION_LEFT}{LEFT_UNREACHABLE}")
                ),
                1
            );
            assert!(rig.finished(&run));
        }

        #[test]
        fn a_finish_that_waits_on_the_session_end_marks_the_pull_request_ready_once() {
            let (mut rig, run, hub) = built();
            place(&mut rig, &run, 1, 1);
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 0)]);
            rig.worker.on_tail(Ok(PASS_TAIL.into()));
            for _ in 0..3 {
                rig.worker
                    .on_end_sessions(Err(WorkerError::Unreachable("no route".into())));
            }
            assert_eq!(go(&rig, &run), GateStep::NotReady);
            assert!(!hub.lock().pulls[&12].draft);
            // A person puts it back to draft while the end of the session is waited for.
            hub.set_draft(true);
            assert_eq!(go(&rig, &run), GateStep::NotReady);
            assert_eq!(go(&rig, &run), GateStep::NotReady);
            let readies = |rig: &Rig| {
                rig.sim
                    .writes()
                    .iter()
                    .filter(|write| write.contains("pr ready 12"))
                    .count()
            };
            assert_eq!(readies(&rig), 1, "the ready mark is asked for once");
            assert!(hub.lock().pulls[&12].draft, "a person's draft stands");
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
            assert_eq!(readies(&rig), 1);
        }

        #[test]
        fn a_second_attempt_after_the_first_was_marked_ready_is_marked_ready_again() {
            let (mut rig, run, hub) = built();
            place(&mut rig, &run, 1, 1);
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 0)]);
            rig.worker.on_tail(Ok(PASS_TAIL.into()));
            // Attempt 1 is marked ready and its label move fails.
            rig.sim.fail("issue edit", 1);
            assert!(try_go(&rig, &run).is_err());
            assert!(has_step(&rig, &run, &format!("{GATE_READY_MARKED}1")));
            assert_eq!(rig.sim.labels(), ["factory:building"]);
            // The head moves: back to draft, and attempt 2 gates the new head and passes.
            hub.set_head(MOVED);
            assert_eq!(go(&rig, &run), GateStep::NotReady);
            assert!(hub.lock().pulls[&12].draft);
            rig.worker.on_create(Ok(gate_ws(2)));
            assert_eq!(go(&rig, &run), GateStep::Running);
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(2, 0)]);
            rig.worker.on_tail(Ok(PASS_TAIL.into()));
            assert_eq!(go(&rig, &run), GateStep::Passed { pr: 12 });
            let readies = rig
                .sim
                .writes()
                .iter()
                .filter(|write| write.contains("pr ready 12") && !write.contains("--undo"))
                .count();
            assert_eq!(readies, 2, "each attempt's pass marks it ready");
            assert!(!hub.lock().pulls[&12].draft);
            assert_eq!(rig.sim.labels(), ["factory:pr-open"]);
        }
    }
}

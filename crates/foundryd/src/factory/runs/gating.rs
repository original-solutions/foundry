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
//!    Then a session still at work (starting, working, waiting for a person, or
//!    unreachable), one with no turn record, one that has run no turn since a failure was sent
//!    under the newest, and a run with no pull request reported are [`GateStep::NotReady`].
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
//!    `gate_retries`, and the attempt keeps its number. A terminal that exited is read once:
//!    the worker's tail is read and `gate-ended <k> <exit code>` and the tail go in one step
//!    (the tail cut so the step, header included, is within the ledger's 64 KB: the header is
//!    the verdict). Then the gate's workspace is archived by force (`gate-archiving <k>`, the
//!    call, `gate-archived <k>`), `gate_command` lines (one for each command, as the exit code
//!    says) and `gate_finished` are written, and the verdict is acted on.
//! 6. **Act.** The pull request is read again. A head that is not the one gated is
//!    `gate-head-moved <attempt> <head>`, whichever the verdict, and the next pass gates the
//!    new head as the next attempt; a pull request merged or closed meanwhile ends the run
//!    first (a merge of the gated head records the pass before it ends). The attempt number
//!    is the count of gates that gave a verdict or a moved head, plus one. `gate_retries` is
//!    one counter per round: attempt `a` has spent `a - 1`, and an attempt past
//!    `gate_retries + 1` is not placed, whether the gate before it failed or the head moved.
//!    - **Fail.** A pull request someone took out of draft goes back to draft first
//!      (`pr_set_draft`). With a retry left the failure is sent to the session once, as the
//!      runner sends it (`gate-sending <attempt> <turn>`, `send_once` under the key
//!      `gate-failure <attempt> <turn>`, `gate_sent_back`), and the pass is
//!      [`GateStep::SentBack`]; the session works, `building` reads its next turn and the gate
//!      comes back with the next attempt. With none left the run is stuck
//!      (`gate-retries-used-up`) with the factory's fixed line in the comment and the output
//!      in the ledger only.
//!    - **Pass.** `gate_command` lines then `gate-passed <head> <count>` (cause `attempt`),
//!      recorded before anything is marked ready. With `required_checks`, the checks on that
//!      commit are read (`checks_read`): all passed finishes; one failed is sent back at once
//!      as a gate failure (`messages::check_failed`, key `gate-failure <attempt> <turn>`),
//!      stuck when no retry is left; pending or missing is [`GateStep::ChecksPending`] until
//!      `limits.checks_minutes` have passed since the `gate-passed` line, then stuck
//!      (`checks-timed-out`, the read recorded first). A head that moved while the checks were
//!      waited for is gated again, like any moved head, instead of stuck.
//! 7. **Finish a pass.** The build session is ended (`end_sessions`, recorded as
//!    `build-session-ended`) before any GitHub write; then the pull request is marked ready,
//!    the results are posted once, the label moves to `pr-open` and the status comment is
//!    written (`apply_actions` of `pr-ready`, so `state_changed` is last); then the build
//!    workspace is archived by force (`build-archiving`, the call, `build-archived`), after
//!    the label, because a branch already on `origin` is all that is left to keep and an
//!    archive before it would leave a `building` run with no workspace if a write failed. The
//!    pass is [`GateStep::Passed`] once all of it is done; a pass cut short or failing in the
//!    archive finishes it on the next call, which asks for no end of sessions, ready, comment
//!    or label again.
//!
//! The output tail kept in the event is cut by the ledger to 64 KB; the text sent to the
//! session is `messages::gate_failed` over the tail, and the stuck comment is
//! `messages::stuck`, as the other steps do.
//!
//! The wait for `required_checks` is measured on the timestamps the ledger's events carry, as
//! `limits.minutes` is ([`building::time_limit`](super::building::time_limit)): the
//! `gate-passed` line's, against "now" ([`advance`] reads the system's wall clock on the
//! ledger's stamping base; [`advance_at`] is given it), through `elapsed_ms`, so a wall clock
//! that stepped back adds no time. The sleep-counting clock `ctx.clock` is not read: its
//! readings mean nothing to a coordinator that took over, and the wait must survive one. The
//! spec's own `limits.minutes` rule, which excludes the checks' wait from the time a run has
//! worked, rests on the same timestamps (`Seen::worked_ms`), so the two never disagree about
//! when the wait began.
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
//!   with exit code 0 and no duration, and a failed one a single line for the command the tail
//!   names (the last command when it names none), with the terminal's exit code.
//! - A head that moved is gated again and spends a retry (spec); the runner's gate left it for
//!   the next call under the same attempt, and its checks wait made it stuck
//!   (`checks-head-moved`). [`HEAD_KEPT_MOVING`] is the stop when none is left.
//! - A gate that could not run backs off by passes and never counts as an attempt; the runner
//!   had no such failure of its own.
//! - A failure sent back, and a stuck comment, say [`CONFIG_DIFFERS`] when `.dw/config.yml` at
//!   the head differs from the default branch's (`gate::config_differs`).
//! - The session is ended and its workspace archived by force at the pass, where the runner
//!   archived the thread and removed its worktree and left a `worktree_removed` line: Foundry
//!   writes no such line, and a workspace that cannot be archived is an error the next pass
//!   asks again, with the label already moved.
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
//!   (`removeLeftovers`, `gate_cleanup_failed`) and worktree removal (`worktree_removed`):
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
//! - The runner's `command id` cases: `send_once`'s key and prompt count replace the id.

use foundry_proto::factory::{RunEvent, RunId};
use foundry_proto::settings::SharedSettings;

use super::{RunContext, RunError};
use crate::factory::placing::Slots;

/// The step written before a gate workspace is asked for, recording the commit gated:
/// `gate-requested <attempt> <k> <head> <base tip> <machine>`.
pub const GATE_REQUESTED: &str = "gate-requested ";
/// The step written once the worker has answered the create: `gate-created <k> <workspace id>`.
pub const GATE_CREATED: &str = "gate-created ";
/// The step that records a gate's verdict and tail: `gate-ended <k> <exit code>` on its first
/// line, then the setup terminal's tail (its last bytes, the step within the ledger's limit).
pub const GATE_ENDED: &str = "gate-ended ";
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
/// The step written once the build session is ended at the pass.
pub const BUILD_SESSION_ENDED: &str = "build-session-ended";
/// The step written before the build workspace is archived at the pass.
pub const BUILD_ARCHIVING: &str = "build-archiving";
/// The step written once it was.
pub const BUILD_ARCHIVED: &str = "build-archived";

/// The public reason of a run whose pull request head kept moving until no retry was left.
pub const HEAD_KEPT_MOVING: &str = "the pull request's head kept moving, so the gate could not settle on a commit within gate_retries";

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

/// One pass over `run`, whose session reported its pull request. See the module's words.
/// "Now" is the system's wall clock on the ledger's stamping base.
pub fn advance(
    ctx: &RunContext<'_>,
    run: &RunId,
    shared: &SharedSettings,
    slots: &mut Slots,
) -> Result<GateStep, RunError> {
    advance_at(ctx, run, shared, slots, crate::factory::ledger::system_ms())
}

/// [`advance`] with "now" given, in Unix epoch milliseconds on the ledger's stamping base.
pub fn advance_at(
    ctx: &RunContext<'_>,
    run: &RunId,
    shared: &SharedSettings,
    slots: &mut Slots,
    now_ms: u64,
) -> Result<GateStep, RunError> {
    let _ = (ctx, run, shared, slots, now_ms);
    todo!("the gating step")
}

/// Whether `events` show a gate in flight, which holds one of the fleet's gate slots: placed
/// (`gate-requested`) and neither archived (`gate-archived`) nor given up on
/// (`gate-could-not-run`) since.
pub fn gate_running(events: &[RunEvent]) -> bool {
    let _ = events;
    todo!("the gate slot")
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
    use crate::factory::ledger::testing::T0;
    use crate::factory::lifecycle::State;
    use crate::factory::messages::{
        self, CommandFailure, FailedCheck, GateFailure, SESSION_GONE, STOPPED_BY_HAND,
    };
    use crate::factory::worker::{
        AlwaysHeld, Call, FleetView, FleetViewBuilder, Lost, WorkerError,
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
    const MINUTE: u64 = 60_000;

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
                        "isCrossRepository": false,
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
        let mut view = gate_machine(FleetView::builder()).session(session_record(state));
        if turn > 0 {
            view = view.turn(SessionTurn {
                session: session(),
                workspace: workspace(),
                project: project_id(),
                turn,
                prompts: turn,
                question: false,
            });
        }
        for (workspace, terminal) in gate_records(run, gates) {
            view = view.workspace(workspace).terminal(terminal);
        }
        rig.view = view.build();
    }

    /// As [`show`], with the gate's machine offline for `span`.
    fn show_gone(rig: &mut Rig, run: &RunId, turn: u64, gates: &[G], span: Duration) {
        let mut view = FleetView::builder()
            .online(builder())
            .session(session_record(SessionState::Idle))
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
        let mut rig = Rig::new();
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
                branch: Some(BRANCH.into()),
                model_instance: None,
                model: Some("opus".into()),
                model_options: None,
                resumed_from: None::<ResumedFrom>,
                base: Some("main".into()),
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

    fn now(rig: &Rig, run: &RunId) -> u64 {
        rig.events(run).last().map_or(T0, |event| event.ms) + 1_000
    }

    fn open_slots() -> Slots {
        Slots {
            runs_free: 1,
            gates_free: 1,
        }
    }

    fn try_go(rig: &Rig, run: &RunId) -> Result<GateStep, RunError> {
        advance_at(&rig.ctx(), run, &shared(), &mut open_slots(), now(rig, run))
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
        assert!(!calls(rig).contains(&archive_of(workspace())));
    }

    // ----- the runner's `gate: the session's PR becomes ready, or it does not` -----

    mod gate_the_sessions_pr_becomes_ready_or_it_does_not {
        use super::*;

        fn the_gate_request(rig: &Rig, run: &RunId) -> Call {
            Call::Create {
                machine: machine(),
                request: gate::request(&rig.config, project_id(), HEAD, run, 1),
            }
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
            let last = rig.events(&run).pop().unwrap();
            assert_eq!(last.data["to"], "pr-open");
        }

        #[test]
        fn a_gate_pass_archives_the_session_and_removes_its_worktree_before_the_issue_is_pr_open() {
            let (mut rig, run, _hub) = built();
            place(&mut rig, &run, 1, 1);
            // The session cannot be ended: nothing is written on GitHub, the label stays, and
            // the next pass ends it and goes on.
            rig.worker
                .on_end_sessions(Err(WorkerError::Failed("the machine is busy".into())));
            show(&mut rig, &run, SessionState::Idle, 1, &[exited(1, 0)]);
            rig.worker.on_tail(Ok(PASS_TAIL.into()));
            assert!(matches!(try_go(&rig, &run), Err(RunError::Worker(_))));
            no_writes(&rig);
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
                SessionState::Unreachable,
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
        fn a_session_that_is_gone_at_gate_time_sends_the_issue_to_stuck() {
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
        fn the_second_failure_is_sent_with_its_own_key_and_the_turn_it_read() {
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
        fn a_send_that_fails_leaves_the_attempt_open_and_a_retry_sends_under_the_same_key() {
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
            advance_at(&rig.ctx(), run, &shared(), &mut none, now(rig, run)).unwrap()
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
        fn a_mergeable_answer_runs_the_gates_own_checkout() {
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

        fn passed_at(rig: &Rig, run: &RunId) -> u64 {
            rig.events(run)
                .iter()
                .find(|event| {
                    event.event == "step_changed"
                        && event.data["step"]
                            .as_str()
                            .is_some_and(|step| step.starts_with("gate-passed "))
                })
                .expect("a recorded pass")
                .ms
        }

        fn go_at(rig: &Rig, run: &RunId, now_ms: u64) -> Result<GateStep, RunError> {
            advance_at(&rig.ctx(), run, &shared(), &mut open_slots(), now_ms)
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
            let late = passed_at(&rig, &run) + 29 * MINUTE;
            assert_eq!(go_at(&rig, &run, late).unwrap(), GateStep::ChecksPending);
            let late = passed_at(&rig, &run) + 31 * MINUTE;
            let step = go_at(&rig, &run, late).unwrap();
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
            let late = passed_at(&rig, &run) + 31 * MINUTE;
            let step = go_at(&rig, &run, late).unwrap();
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
            let late = passed_at(&rig, &run) + 31 * MINUTE;
            // The lease is lost after the read is recorded and before the stuck path ends the
            // session.
            rig.guard = Box::new(Lost);
            assert!(matches!(
                go_at(&rig, &run, late),
                Err(RunError::LeaseLost(_))
            ));
            assert_eq!(event_count(&rig, &run, "checks_read"), 1);
            rig.guard = Box::new(AlwaysHeld(1));
            hub.set_checks(HEAD, GREEN);
            let step = go_at(&rig, &run, late).unwrap();
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
            let late = passed_at(&rig, &run) + 31 * MINUTE;
            assert_eq!(
                go_at(&rig, &run, late).unwrap(),
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
        fn a_failed_check_waits_when_the_session_has_no_turn_record() {
            let (mut rig, run, hub) = built();
            gated_with(&mut rig, &run, &hub, BUSY);
            hub.set_checks(HEAD, RED);
            show(&mut rig, &run, SessionState::Idle, 0, &[]);
            assert_eq!(go(&rig, &run), GateStep::NotReady);
            assert_eq!(sends(&rig), vec![]);
        }

        #[test]
        fn a_failure_that_may_not_have_reached_the_session_is_sent_again_under_the_same_key() {
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
            let late = passed_at(&rig, &run) + 31 * MINUTE;
            assert_eq!(go_at(&rig, &run, late).unwrap(), GateStep::NotReady);
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
        fn ends_the_run_writes_nothing_on_github_runs_no_command_and_ends_the_session_once() {
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
        fn whose_end_of_session_fails_is_an_error_with_nothing_written_and_the_next_call_ends_it() {
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

        fn archived(rig: &Rig) -> Vec<WorkspaceId> {
            calls(rig)
                .into_iter()
                .filter_map(|call| match call {
                    Call::Archive { workspace } => Some(workspace),
                    _ => None,
                })
                .collect()
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
            assert_eq!(rig.sim.labels(), ["factory:stuck"]);
            assert!(
                rig.sim
                    .comments()
                    .iter()
                    .any(|comment| comment.contains("the gate could not run"))
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
            assert_eq!(go(&rig, &run), GateStep::BackingOff { passes_left: 0 });
            rig.worker.on_create(Ok(gate_ws(2)));
            assert_eq!(go(&rig, &run), GateStep::Running);
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
                .on_archive(Err(WorkerError::Unreachable("no route".into())));
            assert!(matches!(try_go(&rig, &run), Err(RunError::Worker(_))));
            assert!(has_step(&rig, &run, &format!("{GATE_ENDED}1 101\n")));
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
            let kept = rig
                .steps(&run)
                .into_iter()
                .find(|step| step.starts_with(GATE_ENDED))
                .expect("the verdict and its tail");
            assert!(kept.starts_with(&format!("{GATE_ENDED}1 101\n")));
            assert!(kept.len() <= 64 * 1024, "{}", kept.len());
            assert!(kept.ends_with("setup failed: cargo test exited 101\n"));
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
            let step = advance_at(&rig.ctx(), &run, &shared(), &mut none, now(&rig, &run));
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
            let at = now(&rig, &run);
            rig.worker.on_create(Ok(gate_ws(1)));
            let first = advance_at(&rig.ctx(), &run, &shared(), &mut slots, at).unwrap();
            let again = advance_at(&rig.ctx(), &second, &shared(), &mut slots, at).unwrap();
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
}

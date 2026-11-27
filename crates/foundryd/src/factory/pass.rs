//! One pass over one project.
//!
//! Ported from the runner's `src/pass.ts`, as far as it applies to one project whose lease this
//! machine holds. [`run`] is one pass, in the order the plan gives:
//!
//! 1. The project's `.dw/config.yml` is read from its default branch through the tracker, and
//!    read again only when the branch's head moved ([`Memo`] keeps the copy and the head). A
//!    file that is missing or does not load leaves the project idle, with the reason in its
//!    status.
//! 2. Without the lease (the guard fails, or the lease task's newest tick says another machine
//!    holds it, or that no holder is known) the pass writes a not-holding status and stops. A
//!    project whose login this machine holds no token for writes "no token" and touches
//!    nothing on GitHub: it is checked before step 1.
//! 3. The runs are found: the ledger's unfinished runs, the runs Foundry's claim comments name
//!    on issues in `triage` or `building` that have no confirmed run, and the runs the fleet's
//!    workspace names (`fac/<N>-<attempt>`) name on such issues, before any event. A run with
//!    no event is carried on by `start::advance`, which reads the claim comment (the takeover).
//! 4. An issue in `triage` or `building` whose newest claim comment by the project's login is
//!    not Foundry's (the runner's marker, or a Foundry marker that does not parse), and that no
//!    run of Foundry's holds, is a foreign claim: listed, and left alone. An issue with no claim
//!    comment of the login and no workspace of a run is neither: it is left alone and not
//!    listed, since nothing claimed it (a marker in another login's comment counts for nothing).
//! 5. Every run is advanced through tasks 22 to 28, routed as each step's module says the pass
//!    must ([`work_run`]): a stop asked for first, the gate's runs to the gate, the time limit,
//!    the resume before the start and the build, the reconcile before the build, the build
//!    and its forward, and the gate in the same pass as a pull request is reported. A run whose
//!    end the reconcile is winding up is touched by no other step that pass. Runs in `pr-open`,
//!    and runs a feedback round owns, have their feedback step after the other runs' steps and
//!    before the claims, as the issue numbers them (the runner's `prOpenRuns` came after its
//!    claim; the issue's order decides).
//! 6. Ready issues are claimed, oldest first (by number, as the runner orders them), while a run
//!    slot is free, claims are not paused and `placing::place_run` has a machine. An issue
//!    labelled `postponed`, one whose older run is gating, and one blocked by an open issue
//!    (a dependency link, else its `## Depends on` text) is not claimed. Each waits in the
//!    status's ready queue with its reason.
//! 7. A machine heard from again (online now, not at the previous pass) has the orphans of
//!    task 24 archived by force.
//! 8. The holding status is written.
//!
//! A tracker error ends the project's pass where it happened: what was recorded stays recorded
//! and is carried on next pass, as each step records before it acts. Three failed passes
//! running set the status's idle reason; the next pass that ends well clears it. A lost lease
//! ends the pass at once, with no further change on GitHub or of a worker, and writes a
//! not-holding status. Any other error of one run's step is noted against that run and the
//! pass goes on.
//!
//! The run and gate slots are the fleet's (`max_runs`, `max_gates`), and the projects' passes
//! run at once, each on its own thread. [`Book`] is what they share: each project's entry holds
//! the slots its runs hold and the machines it placed runs on whose sessions the machines'
//! samples do not count yet. A claim, a gate and a feedback round take their slot in the book,
//! under its lock, only while the fleet's count is under its limit, and give it back when they
//! did not spend it, so two passes never both take the last slot.
//!
//! Where it differs from the runner:
//!
//! - One project per pass, not every watched repo: the service runs one pass per project
//!   whose lease this machine holds, at once, so a slow `gh` in one project holds up no other.
//!   The runner's lines on standard output, its machine log (`pass_started`, `claim_skipped`,
//!   `ready_listed`, `pass_finished`) and its attention items are the status this pass writes,
//!   whose idle reason, ready queue, held-up runs and foreign claims the attention list reads.
//! - The runner's `max_concurrent_runs` is the fleet's `max_runs`, counted across projects.
//! - A claim is made only when placement has a machine now (F7), so a claim never waits for a
//!   slot in `triage`; with no machine no more issues of the project are claimed this pass.
//! - Claims cut short leave the issue `ready` (Foundry's comment comes first), so the runner's
//!   move of an unassigned claim to `factory:proposed` (`unassignedClaims`) has nothing to
//!   repair, and is not ported. An issue in `triage` or `building` that no Foundry claim or
//!   run holds is listed as a foreign claim and left alone.
//! - The runner's give-way between machines (`yieldedRuns`, `claim gave way`) is not ported: one
//!   coordinator holds the project's lease.
//! - A config is read again only when the default branch's head moved; a read that fails is a
//!   tracker error and ends the pass. The runner's "last good copy" of a config and a login
//!   whose re-read blipped is the copy [`Memo`] keeps for an unmoved head.
//! - A tracker error ends the project's pass (the plan's rule), where the runner noted it and
//!   went on to the next issue or repo. These runner cases are ported under their names with
//!   the plan's outcome, the pass ended and the next pass going on:
//!   `an issue whose blockers cannot be read is not claimed and the pass goes on`, `an issue
//!   whose depends on cannot be read is not claimed and the pass goes on`, `triage issues that
//!   cannot be listed are an error and the others go on` and `a repo whose ready issues cannot
//!   be listed is an error and the others go on` (the others are the other projects, whose
//!   passes run apart: the service's test
//!   `a_tracker_error_in_one_project_ends_its_pass_and_the_next_project_still_runs`). `a failed
//!   start is an error line and the next issue is tried` keeps the runner's outcome for a
//!   worker's failure, which is the run's, not the pass's.
//! - The issue of a stuck run that keeps a workspace is read by the finished-run sweep, every
//!   [`STUCK_READ_EVERY`]; an unfinished stuck path is reconciled every pass, so a path cut
//!   short is finished at once.
//!
//! Runner cases of `pass.test.ts` that have no Rust test of the same name, by group, with the
//! reason. The cases not listed are ported under their names.
//!
//! - Every case of `the Paradev hold`, `the host loads`, `the host checks`, `the cloud proxy
//!   check`, `low disk space` and `high load` (hosts, cloud and the Paradev hold are left out
//!   by the plan; a machine's disk and load are placement's limits, task 25), and `pruning t3/*
//!   branches`, `threads on a broken worktree` and `sessions that ended badly` (T3 threads and
//!   its machine log; a session's end is the resume step's, task 24).
//! - `a provider rate limit` and every case of `cooldown.test.ts`: `src/cooldown.ts` keeps a
//!   window per model across T3 providers and falls back to another provider's model. A
//!   Foundry session starts on the fleet's one model through the gateway, with no fallback to
//!   choose, so nothing of it applies: no rate limit is decided by a pass.
//! - `what the pass records`, `text that is printed`, `the limit, said out loud` and `attention
//!   items`: the runner's printed lines, machine-log events and attention-item store. Foundry's
//!   pass writes its status; the cases that say what waits and why are ported against it
//!   (`the_limit_said_out_loud::issues_left_unclaimed_because_the_limit_is_reached_wait_with_why`).
//! - `a repo whose config or login could not be re-read` and `a repo whose config is not loaded
//!   pass after pass`, but for `three_stale_passes_in_a_row_raise_the_item_and_the_next_good_load_clears_it`
//!   and `a_repo_whose_config_cannot_be_loaded_is_reported_and_skipped_while_others_go_on`:
//!   Foundry keeps the copy of an unmoved head, has one login per project and counts failed
//!   passes, not skipped repos.
//! - `which repo gets a free slot`: one project per pass; the fleet's slots are shared through
//!   [`Book`] and taken in the order the passes reach them (`the_book::two_projects_never_both_take_the_last_run_slot`).
//! - The cases about printed lines with nothing behind them in Foundry: `a cut ready listing is
//!   said once and the issues read are still claimed`, `a cut triage listing is said once`, `a
//!   pass that lists no repo's ready issues writes no ready_listed`, `with the pause a pass with
//!   nothing ready still says it once`, `a claimed issue that went stuck says so and names the
//!   run` and `a triage run that went stuck says so and names the run` (a launch that goes
//!   stuck is `start`'s, task 22).
//! - `a repo whose tracker login cannot be read is logged and skipped, and the others still
//!   run`: a project's login is a shared setting, not read from GitHub; a login with no token
//!   is the "no token" status (`the_lease::a_project_with_no_token_writes_no_token_and_touches_nothing_on_github`).
//! - `a ledger that cannot be listed is an error and nothing is claimed` and `a run whose file
//!   cannot be read is an error line and counts as active`: the ledger is the store, which the
//!   fakes cannot make fail. A store error ends the pass (`Ended::Failed`), and a run whose
//!   events cannot be read is noted and holds a slot by `holds_slot`.
//! - `a repo two T3 projects name is watched from neither: one error line, one event, the rest go
//!   on` (T3 projects) and `an issue blocked by its text is recorded as a skipped claim` (a
//!   machine-log event; the status's ready queue says it is blocked, see
//!   `each_blocker_from_the_text_carries_the_suffix_and_one_in_another_repo_names_its_repo`).
//! - `a start that finds a gate running says so and is no error`: the pass does not claim an
//!   issue whose older run is gating (`a_ready_issue_whose_older_run_is_gating`), so `start`
//!   never meets the gate there.
//! - `several machines on one repo` and `a repo whose factory.owner is another machine`: one
//!   coordinator acts, by the lease (F9: `factory.owner` is ignored and the status says so,
//!   `a_repo_that_sets_factory_owner_is_claimed_from_and_the_status_says_it_is_ignored`).
//! - `a pass that has run long`: the runner's pass had a 30-minute timeout across every repo;
//!   Foundry's pass has none, and every `gh` call ends in 30 seconds.
//! - `following a building run`, `runs that are confirmed and still in triage`, `reading the
//!   ledger`, `pr-open runs: ...`: each step's own cases are its module's (tasks 22 to 28);
//!   the routing the pass adds is ported where it changes what the pass does
//!   (`a_triage_run_is_worked_before_new_issues_are_claimed`,
//!   `a_session_that_reports_its_pull_request_has_its_gate_started_in_the_same_pass`).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use foundry_proto::factory::{
    FactoryBody, FactoryRecord, FactoryStatus, FinishedRun, ForeignClaim, HeldUp, HeldUpKind,
    LiveRun, NotHolding, RunId, RunOutcome, Waiting,
};
use foundry_proto::ids::{MachineId, ProjectId, SessionId, WorkspaceId};
use foundry_proto::settings::SharedSettings;

use super::config::{self, ProjectConfig};
use super::github::{Claim, Commenting, IssueSummary, TrackerError, newest_claim};
use super::lease::LeaseState;
use super::ledger::Ledger;
use super::ledger::events::names;
use super::ledger::list::{RunInfo, holds_slot};
use super::lifecycle::State;
use super::placing::{self, Reserved, Slots};
use super::runs::building::{self, BuildStep, LimitStep};
use super::runs::feedback::{self, FeedbackStep};
use super::runs::gating::{self, GateStep};
use super::runs::progress::read_progress;
use super::runs::resume::{self, ResumeStep};
use super::runs::start::{self, ClaimOutcome, StartStep};
use super::runs::stop::{self, ReconcileStep, StopStep};
use super::runs::{RunContext, RunError, RunProject, RunSettings, run_config};
use super::status::{self, PassResults};
use super::worker::{FleetView, Guard, Worker};
use crate::clock::Clock;
use crate::store::{Applied, Store, StoreError};

/// Passes that fail in a row before the status says the factory is idle.
pub const IDLE_AFTER_FAILURES: u32 = 3;

/// How often the finished runs are swept by the reconcile, which reads the issue of a stuck run
/// that keeps a workspace: the runner's `STUCK_READ_EVERY_MS`.
pub const STUCK_READ_EVERY: Duration = Duration::from_secs(10 * 60);

/// The label that parks a ready issue: matched in any case, as the whole name.
pub const POSTPONED: &str = "postponed";

/// What a ready issue whose older run is gating waits for.
pub const GATE_RUNNING: &str = "the gate of an earlier run of the issue is running";

/// What a ready issue waits for while an earlier run of it has events this build cannot read.
pub const UNREADABLE_RUN: &str = "an earlier run's events cannot be read by this build";

/// What the sweep says of a run whose events this build cannot read.
const UNREADABLE_SWEPT: &str = "its events cannot be read by this build: the sweep left it";

/// What a ready issue waits for when no run slot is free.
pub const NO_SLOT: &str = "no run slot is free";

/// What the pass reads and calls for one project. Everything is borrowed for one pass.
pub struct World<'a> {
    /// The project, its repo and its login.
    pub project: &'a RunProject,
    /// What the lease task's newest tick said.
    pub lease: &'a LeaseState,
    /// Whether the lease is held now: checked before every change.
    pub guard: &'a dyn Guard,
    /// GitHub as the project's login, with the machine's hider.
    pub tracker: &'a dyn Commenting,
    pub worker: &'a dyn Worker,
    /// What the fleet shows this pass.
    pub view: &'a FleetView,
    /// The ledger; the pass writes under the generation the guard answers.
    pub ledger: &'a Ledger,
    /// Where the status is written.
    pub store: &'a Mutex<Store>,
    /// This machine.
    pub me: MachineId,
    /// The shared settings: the fleet's and the project's factory settings, the limits.
    pub shared: &'a SharedSettings,
    /// What every project's pass shares: the fleet's slots and reservations.
    pub book: &'a Book,
    /// The clock that counts through sleep.
    pub clock: &'a dyn Clock,
}

/// What one project's passes keep from one to the next.
#[derive(Debug, Default)]
pub struct Memo {
    /// The config read, and the commit of the default branch it was read at.
    config: Option<(String, Result<ProjectConfig, String>)>,
    /// Passes that failed in a row, and why the last one did.
    failures: u32,
    failed: Option<String>,
    /// The writer's pass count; read from the store's own status at the first pass.
    pass: Option<u64>,
    /// What the last pass that ended well found, written again by a pass that fails.
    last: Option<PassResults>,
    /// The machines online at the previous pass; `None` before the first.
    online: Option<BTreeSet<MachineId>>,
    /// An orphan's archive failed: the next pass looks for orphans again.
    orphans_owed: bool,
    /// When the finished runs were last swept, on the clock.
    swept: Option<Duration>,
    /// A due sweep began and has not gone through every run: the next pass is its one retry.
    sweep_unfinished: bool,
    /// The run whose reconcile ended the last sweep part way: the next sweep goes on from the
    /// run after it, so a run that always fails does not keep the runs after it unswept.
    sweep_after: Option<RunId>,
    /// The runs seen finished, by run id: a finished run's summary never changes, so its
    /// events are read once. `None` for one this build cannot show.
    finished: BTreeMap<RunId, Option<Done>>,
}

/// A finished run as the status shows it, with where its `finished` event stands.
#[derive(Debug)]
struct Done {
    /// The lease generation, the record's version and the writer of the `finished` event:
    /// the order the runs' events put it in, never the wall clock. `None` when not read.
    at: Option<(u64, u64, MachineId)>,
    run: FinishedRun,
    /// Why a run that finished `stuck` or `needs-info` waits for a person.
    reason: Option<String>,
}

/// What one pass did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PassReport {
    /// The pass held the lease, at this generation.
    pub generation: Option<u64>,
    /// The runs claimed, in order.
    pub claimed: Vec<RunId>,
    /// What each run's steps did, by run.
    pub runs: Vec<(RunId, String)>,
    /// The ready issues not claimed, with why.
    pub waiting: Vec<Waiting>,
    /// The foreign claims found.
    pub foreign: Vec<ForeignClaim>,
    /// The orphans archived.
    pub archived: Vec<WorkspaceId>,
    /// The error that ended the pass, when one did.
    pub failed: Option<String>,
    /// The lease was lost during the pass.
    pub lease_lost: bool,
    /// The status written.
    pub status: Option<FactoryStatus>,
}

/// The fleet's slots and reservations, shared by every project's pass.
///
/// Each project's entry is what its runs hold: set from its ledger at the start and the end of
/// each pass ([`Book::begin`], [`Book::finish`]), and raised in between, under the book's lock,
/// by each slot a claim, a gate or a feedback round takes ([`Book::take_run`],
/// [`Book::take_gate`]): a slot is taken only while the whole fleet's count is under its
/// limit, so two projects' passes never both take the last one. A project's entry is seeded
/// from its ledger when the service starts it ([`seed`]), so its runs count before its first
/// pass has begun. A pass holds a [`Ticket`]; once its project is forgotten (turned off, or
/// this machine stops acting) or seeded again (started again) the ticket is stale and nothing
/// it does changes the book.
#[derive(Debug, Default)]
pub struct Book {
    state: Mutex<BookState>,
}

#[derive(Debug, Default)]
struct BookState {
    held: BTreeMap<ProjectId, Held>,
    /// Raised by every [`Book::forget`], so a pass begun before it changes nothing after it.
    epochs: BTreeMap<ProjectId, u64>,
}

/// What one project holds of the fleet.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Held {
    runs: u32,
    gates: u32,
    /// The machines the project placed runs on whose sessions have not started.
    placed: Vec<MachineId>,
}

/// One pass's hold on its project's entry in the [`Book`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ticket {
    project: ProjectId,
    epoch: u64,
}

impl BookState {
    fn current(&self, ticket: Ticket) -> bool {
        self.epochs.get(&ticket.project).copied().unwrap_or(0) == ticket.epoch
    }

    fn total(&self, of: impl Fn(&Held) -> u32) -> u32 {
        self.held
            .values()
            .fold(0u32, |sum, held| sum.saturating_add(of(held)))
    }
}

impl Book {
    fn lock(&self) -> std::sync::MutexGuard<'_, BookState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The ticket of a pass of `project` that is about to check its lease. Taken before that
    /// check, so a project forgotten after it (the service releases the lease, then forgets)
    /// leaves the ticket stale.
    fn ticket(&self, project: ProjectId) -> Ticket {
        let epoch = self.lock().epochs.get(&project).copied().unwrap_or(0);
        Ticket { project, epoch }
    }

    /// The ticket's pass begins, its project holding `held`: its entry is set, and the
    /// machines every other project placed runs on are answered. `None` for a stale ticket,
    /// which writes nothing.
    fn begin(&self, ticket: Ticket, held: Held) -> Option<Vec<MachineId>> {
        let mut state = self.lock();
        if !state.current(ticket) {
            return None;
        }
        state.held.insert(ticket.project, held);
        Some(
            state
                .held
                .iter()
                .filter(|(p, _)| **p != ticket.project)
                .flat_map(|(_, held)| held.placed.iter().copied())
                .collect(),
        )
    }

    /// The pass ends, its project holding `held`.
    fn finish(&self, ticket: Ticket, held: Held) {
        let mut state = self.lock();
        if state.current(ticket) {
            state.held.insert(ticket.project, held);
        }
    }

    /// Takes one of the fleet's `max` run slots for the ticket's project, when one is free.
    fn take_run(&self, ticket: Ticket, max: u32) -> bool {
        let mut state = self.lock();
        if !state.current(ticket) || state.total(|h| h.runs) >= max {
            return false;
        }
        let held = state.held.entry(ticket.project).or_default();
        held.runs = held.runs.saturating_add(1);
        true
    }

    /// How many of the fleet's `max` run slots are free now.
    pub(super) fn runs_free(&self, max: u32) -> u32 {
        max.saturating_sub(self.lock().total(|h| h.runs))
    }

    /// Gives back a run slot taken and not spent.
    fn give_run(&self, ticket: Ticket) {
        let mut state = self.lock();
        if state.current(ticket)
            && let Some(held) = state.held.get_mut(&ticket.project)
        {
            held.runs = held.runs.saturating_sub(1);
        }
    }

    /// Takes one of the fleet's `max` gate slots, when one is free.
    fn take_gate(&self, ticket: Ticket, max: u32) -> bool {
        let mut state = self.lock();
        if !state.current(ticket) || state.total(|h| h.gates) >= max {
            return false;
        }
        let held = state.held.entry(ticket.project).or_default();
        held.gates = held.gates.saturating_add(1);
        true
    }

    /// Gives back a gate slot taken and not spent.
    fn give_gate(&self, ticket: Ticket) {
        let mut state = self.lock();
        if state.current(ticket)
            && let Some(held) = state.held.get_mut(&ticket.project)
        {
            held.gates = held.gates.saturating_sub(1);
        }
    }

    /// A run of the ticket's project was placed on `machine` this pass.
    fn place(&self, ticket: Ticket, machine: MachineId) {
        let mut state = self.lock();
        if state.current(ticket) {
            state
                .held
                .entry(ticket.project)
                .or_default()
                .placed
                .push(machine);
        }
    }

    /// The project is about to run here, or to run again: its entry is `held`, what its ledger
    /// holds, and a pass begun before this changes nothing after it. Its runs count against
    /// the fleet's limit from now, before its first pass reaches [`Book::begin`], which is the
    /// count every other project's pass claims against.
    fn seed(&self, project: ProjectId, held: Held) {
        let mut state = self.lock();
        state.held.insert(project, held);
        let epoch = state.epochs.entry(project).or_insert(0);
        *epoch = epoch.saturating_add(1);
    }

    /// The project no longer runs here: it holds nothing of the fleet, and a pass of it still
    /// under way changes nothing here.
    pub fn forget(&self, project: ProjectId) {
        let mut state = self.lock();
        state.held.remove(&project);
        let epoch = state.epochs.entry(project).or_insert(0);
        *epoch = epoch.saturating_add(1);
    }
}

/// Why a pass ended before its end.
enum Ended {
    /// The lease was lost: nothing more is changed.
    Lost,
    /// GitHub, or the store, failed: the pass ends where it was.
    Failed(String),
}

impl From<StoreError> for Ended {
    fn from(error: StoreError) -> Self {
        Ended::Failed(format!("the store could not be read: {error}"))
    }
}

impl From<TrackerError> for Ended {
    fn from(error: TrackerError) -> Self {
        Ended::Failed(error.to_string())
    }
}

/// What ends the pass of a run step's error: a lost lease and a tracker or store error do; any
/// other is the run's, noted against it, and the pass goes on.
fn ending(error: RunError) -> Result<String, Ended> {
    match error {
        RunError::LeaseLost(_) => Err(Ended::Lost),
        RunError::Tracker(error) => Err(error.into()),
        RunError::Store(error) => Err(error.into()),
        other => Ok(format!("error: {other}")),
    }
}

/// One pass over the project `world` names. See the module's words.
///
/// A pass that panics is a failed pass like any other: counted, its status written from the
/// last good results, idle once [`IDLE_AFTER_FAILURES`] have failed in a row. The memo is kept.
pub fn run(world: &World<'_>, memo: &mut Memo) -> PassReport {
    let pass = next_pass(world, memo);
    let ticket = world.book.ticket(world.project.id);
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_pass(world, memo, pass, ticket)
    }));
    match ran {
        Ok(report) => report,
        Err(_) => {
            // A step that ran before the panic may have recorded a run that holds a slot: the
            // book is set from the ledger, as after any failed pass. A ledger that cannot be
            // read leaves it to the next pass.
            if let Ok(known) = known_of(world.ledger, &world.project.repo) {
                world.book.finish(ticket, Pass::holding(&known));
            }
            // The panic's own text is left out: it may quote what a call answered.
            failed(
                world,
                memo,
                PassReport::default(),
                pass,
                "the pass panicked".into(),
            )
        }
    }
}

/// A pass that could not begin [`run`], or that died outside it: counted, its status written
/// from the last good results, idle once [`IDLE_AFTER_FAILURES`] have failed in a row, as for
/// a pass that fails inside it. It changes nothing on GitHub and asks nothing of a worker.
pub fn fail(world: &World<'_>, memo: &mut Memo, error: String) -> PassReport {
    let pass = next_pass(world, memo);
    failed(world, memo, PassReport::default(), pass, error)
}

fn run_pass(world: &World<'_>, memo: &mut Memo, pass: u64, ticket: Ticket) -> PassReport {
    let mut report = PassReport::default();
    let project = world.project.id;
    if matches!(world.lease, LeaseState::NoToken) {
        let why = NotHolding::NoToken {
            login: world.project.login.clone(),
        };
        write(
            world,
            &mut report,
            with_pass(status::not_holding(project, why), pass),
        );
        return report;
    }
    // 1. The config, read again only when the default branch's head moved.
    let config = match read_config(world, memo) {
        Ok(config) => config,
        Err(error) => return failed(world, memo, report, pass, error),
    };
    // 2. The lease.
    let Ok(generation) = world.guard.check() else {
        // Only the passes that hold the lease count toward the idle reason.
        memo.failures = 0;
        memo.failed = None;
        not_holding(world, &mut report, pass);
        return report;
    };
    report.generation = Some(generation);
    let config = match config {
        Ok(config) => config,
        Err(idle) => {
            // A config that does not load is the project's, not a failed pass.
            memo.failures = 0;
            memo.failed = None;
            let mut results = PassResults::new(project);
            results.pass = pass;
            results.generation = generation;
            results.idle = Some(world.tracker.hide(&idle));
            memo.last = Some(results.clone());
            write(world, &mut report, status::build(results));
            return report;
        }
    };
    let mut results = PassResults::new(project);
    results.pass = pass;
    results.generation = generation;
    results.owner_ignored = config.owner_set;
    let ledger = world.ledger.at(generation);
    let settings = RunSettings {
        fleet: world.shared.factory.clone().unwrap_or_default(),
        project: world
            .shared
            .project_factory
            .get(&project)
            .cloned()
            .unwrap_or_default(),
    };
    let mut one = Pass {
        world,
        config: &config,
        ledger: &ledger,
        settings: &settings,
        results,
        report,
        memo,
        titles: BTreeMap::new(),
        ticket,
        others: Vec::new(),
        counted: BTreeSet::new(),
    };
    match one.go() {
        Ok(()) => {
            let Pass {
                results,
                mut report,
                memo,
                ..
            } = one;
            memo.failures = 0;
            memo.failed = None;
            memo.last = Some(results.clone());
            memo.online = Some(world.view_online());
            write(world, &mut report, status::build(results));
            report
        }
        Err(Ended::Lost) => {
            let mut report = one.report;
            report.lease_lost = true;
            not_holding(world, &mut report, pass);
            report
        }
        Err(Ended::Failed(error)) => {
            // A step that failed may have recorded a run that holds a slot: the book is set
            // from the ledger before the next pass, so that no other project's pass claims it.
            let _ = one.recount(&mut Reserved::default());
            let Pass { report, memo, .. } = one;
            failed(world, memo, report, pass, error)
        }
    }
}

impl World<'_> {
    /// The machines the view shows online.
    fn view_online(&self) -> BTreeSet<MachineId> {
        let mut online: BTreeSet<MachineId> = self
            .shared
            .machines
            .iter()
            .map(|machine| machine.id)
            .filter(|machine| self.view.online(*machine))
            .collect();
        online.insert(self.me);
        online
    }
}

/// The pass count for this pass: one more than the last, which at the first pass is the count
/// of the newest status this machine wrote for the project, so it rises across a restart.
fn next_pass(world: &World<'_>, memo: &mut Memo) -> u64 {
    let last = *memo.pass.get_or_insert_with(|| {
        let store = world.store.lock().unwrap_or_else(PoisonError::into_inner);
        store
            .statuses(world.project.id)
            .ok()
            .and_then(|statuses| {
                statuses
                    .into_iter()
                    .find(|(writer, _)| *writer == world.me)
                    .map(|(_, status)| status.pass)
            })
            .unwrap_or(0)
    });
    let next = last.saturating_add(1);
    memo.pass = Some(next);
    next
}

fn with_pass(mut status: FactoryStatus, pass: u64) -> FactoryStatus {
    status.pass = pass;
    status
}

/// The not-holding status of this pass, from the lease task's newest tick.
fn not_holding(world: &World<'_>, report: &mut PassReport, pass: u64) {
    let project = world.project.id;
    let status = match world.lease {
        LeaseState::NoToken => status::not_holding(
            project,
            NotHolding::NoToken {
                login: world.project.login.clone(),
            },
        ),
        LeaseState::HeldBy {
            machine,
            generation,
        } => status::not_holding(
            project,
            NotHolding::HeldBy {
                machine: *machine,
                generation: *generation,
            },
        ),
        // A hold that lapsed since the tick: this machine is the holder the ref names.
        LeaseState::Held { generation } => status::not_holding(
            project,
            NotHolding::HeldBy {
                machine: world.me,
                generation: *generation,
            },
        ),
        LeaseState::Unknown { reason } => idle_not_holding(project, (*reason).to_owned()),
    };
    write(world, report, with_pass(status, pass));
}

/// A pass that GitHub or the store ended: counted, and the last good results written again,
/// idle once [`IDLE_AFTER_FAILURES`] passes have failed in a row.
fn failed(
    world: &World<'_>,
    memo: &mut Memo,
    mut report: PassReport,
    pass: u64,
    error: String,
) -> PassReport {
    let error = world.tracker.hide(&error);
    tracing::warn!(project = %world.project.id, "factory: the pass ended: {error}");
    report.failed = Some(error.clone());
    let Ok(generation) = world.guard.check() else {
        // Only the passes that hold the lease count toward the idle reason.
        memo.failures = 0;
        memo.failed = None;
        not_holding(world, &mut report, pass);
        return report;
    };
    memo.failures = memo.failures.saturating_add(1);
    memo.failed = Some(error.clone());
    report.generation = Some(generation);
    let mut results = memo
        .last
        .clone()
        .unwrap_or_else(|| PassResults::new(world.project.id));
    results.pass = pass;
    results.generation = generation;
    if memo.failures >= IDLE_AFTER_FAILURES {
        results.idle = Some(format!(
            "{} passes in a row failed; the last: {error}",
            memo.failures
        ));
    }
    write(world, &mut report, status::build(results));
    report
}

/// A status of `project` that does not hold the lease and says only why it is idle: `reason`.
pub(super) fn idle_not_holding(project: ProjectId, reason: String) -> FactoryStatus {
    let mut status = status::not_holding(
        project,
        NotHolding::NoToken {
            login: String::new(),
        },
    );
    status.not_holding = None;
    status.idle = Some(reason);
    status
}

/// Stores `status` as `me`'s record, at its next factory version.
pub(super) fn store_status(
    store: &mut Store,
    me: MachineId,
    status: FactoryStatus,
) -> Result<(), String> {
    let written = store.own_next_factory_version().and_then(|version| {
        store.apply_factory(
            &FactoryRecord {
                owner: me,
                version,
                body: FactoryBody::Status(status),
            },
            me,
        )
    });
    match written {
        Ok(Applied::Stored) => Ok(()),
        Ok(other) => Err(format!("the status was not stored: {other:?}")),
        Err(error) => Err(format!("the status could not be written: {error}")),
    }
}

/// Writes `status` as this machine's record, and keeps it on the report.
fn write(world: &World<'_>, report: &mut PassReport, status: FactoryStatus) {
    let mut store = world.store.lock().unwrap_or_else(PoisonError::into_inner);
    if let Err(error) = store_status(&mut store, world.me, status.clone()) {
        tracing::warn!(project = %status.project, "factory: {error}");
    }
    report.status = Some(status);
}

/// The project's config: the copy of an unmoved head, else read at the head. `Ok(Err(why))` is
/// a file that is missing or does not load, which leaves the project idle; `Err` is a tracker
/// error, which ends the pass.
fn read_config(
    world: &World<'_>,
    memo: &mut Memo,
) -> Result<Result<ProjectConfig, String>, String> {
    let repo = &world.project.repo;
    let tracker = world.tracker;
    let branch = tracker.default_branch(repo).map_err(|e| e.to_string())?;
    let head = tracker
        .branch_tip(repo, &branch)
        .map_err(|e| e.to_string())?;
    if let Some((at, config)) = &memo.config
        && *at == head
    {
        return Ok(config.clone());
    }
    let config = match tracker.file_at(repo, config::PATH, &head) {
        Ok(text) => config::parse(&text)
            .map_err(|errors| format!("the {} on {branch} does not load: {errors}", config::PATH)),
        Err(TrackerError::NotFound { .. }) => {
            Err(format!("the {} on {branch} is missing", config::PATH))
        }
        Err(error) => return Err(error.to_string()),
    };
    memo.config = Some((head, config.clone()));
    Ok(config)
}

/// One pass that holds the lease.
struct Pass<'a, 'w> {
    world: &'a World<'w>,
    config: &'a ProjectConfig,
    ledger: &'a Ledger,
    settings: &'a RunSettings,
    results: PassResults,
    report: PassReport,
    memo: &'a mut Memo,
    /// Each issue's title, from the listings read.
    titles: BTreeMap<u64, String>,
    /// The pass's hold on the book, taken before the lease was checked.
    ticket: Ticket,
    /// The machines other projects placed runs on, whose sessions have not started.
    others: Vec<MachineId>,
    /// The runs whose machine `reserved` already counts this pass: those the ledger showed
    /// placed when the pass began, and each one the pass placed itself.
    counted: BTreeSet<RunId>,
}

/// Whose claim an issue in `triage` or `building` carries.
enum Found {
    Ours(RunId),
    Foreign,
    Nothing,
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// One unfinished run with what its events say.
struct Known {
    info: RunInfo,
    events: Vec<foundry_proto::factory::RunEvent>,
    seen: super::runs::progress::Seen,
}

/// Sets `project`'s entry of `book` from what its runs in `ledger` hold. The service does this
/// when it starts the project's tasks, whether the first time or after the project's token
/// changed, so that the project's runs are counted before its first pass has read anything.
/// A project that is no longer watched is [`Book::forget`]ten instead.
pub fn seed(
    book: &Book,
    ledger: &Ledger,
    project: ProjectId,
    repo: &str,
) -> Result<(), StoreError> {
    let held = Pass::holding(&known_of(ledger, repo)?);
    book.seed(project, held);
    Ok(())
}

/// The unfinished runs of `repo` in `ledger`, each with its events.
fn known_of(ledger: &Ledger, repo: &str) -> Result<Vec<Known>, StoreError> {
    let mut known = Vec::new();
    for info in ledger.runs(repo)? {
        let events = if info.unreadable {
            Vec::new()
        } else {
            ledger.read(&info.id)?
        };
        let seen = read_progress(&events);
        known.push(Known { info, events, seen });
    }
    Ok(known)
}

/// The context of one run's steps.
fn context<'c>(
    world: &'c World<'_>,
    ledger: &'c Ledger,
    settings: &'c RunSettings,
    config: &'c ProjectConfig,
) -> RunContext<'c> {
    RunContext {
        tracker: world.tracker,
        worker: world.worker,
        view: world.view,
        ledger,
        guard: world.guard,
        project: world.project,
        config,
        settings,
        clock: world.clock,
    }
}

impl<'a, 'w> Pass<'a, 'w> {
    fn ctx(&self, config: &'a ProjectConfig) -> RunContext<'a> {
        context(self.world, self.ledger, self.settings, config)
    }

    fn note(&mut self, run: &RunId, said: impl Into<String>) {
        self.report.runs.push((run.clone(), said.into()));
    }

    fn title(&self, issue: u64) -> String {
        self.titles.get(&issue).cloned().unwrap_or_default()
    }

    /// The unfinished runs of the project, each with its events.
    fn known(&self) -> Result<Vec<Known>, Ended> {
        Ok(known_of(self.ledger, &self.world.project.repo)?)
    }

    /// The book holds what the ledger now says this project's runs hold, and `reserved` gains
    /// the machine of each run the ledger shows placed that it does not count yet. Called
    /// wherever a step may have recorded a run that holds a slot with no slot taken for it (a
    /// claim carried on, a claim that failed after it recorded), so that the count every
    /// project's pass claims against is the ledger's. It only adds: a reservation the pass made
    /// stays, though the ledger no longer shows its run placed (its session has started, or a
    /// feedback round or a resume took it past `triage`), because the machine's samples do not
    /// count that session yet.
    fn recount(&mut self, reserved: &mut Reserved) -> Result<(), Ended> {
        let known = self.known()?;
        for (run, machine) in Self::placed_runs(&known) {
            if self.counted.insert(run) {
                reserved.hold(machine);
            }
        }
        self.world.book.finish(self.ticket, Self::holding(&known));
        Ok(())
    }

    /// The runs placed on a machine whose session has not started, with that machine.
    fn placed_runs(known: &[Known]) -> Vec<(RunId, MachineId)> {
        known
            .iter()
            .filter(|k| {
                k.info.confirmed
                    && !k.info.session
                    && matches!(k.info.state, None | Some(State::Triage))
            })
            .filter_map(|k| {
                let machine = k.seen.machine.as_deref().and_then(machine_id)?;
                Some((k.info.id.clone(), machine))
            })
            .collect()
    }

    /// What this project's runs hold of the fleet, by their events.
    fn holding(known: &[Known]) -> Held {
        let infos: Vec<RunInfo> = known.iter().map(|k| k.info.clone()).collect();
        Held {
            runs: count(infos.iter().filter(|info| holds_slot(info)).count()),
            gates: count(
                known
                    .iter()
                    .filter(|k| gating::gate_running(&k.events))
                    .count(),
            ),
            placed: Self::placed_runs(known)
                .into_iter()
                .map(|(_, machine)| machine)
                .collect(),
        }
    }

    fn go(&mut self) -> Result<(), Ended> {
        let world = self.world;
        let repo = world.project.repo.clone();
        let known = self.known()?;
        // The fleet's slots: this project's runs hold what their events say, and every other
        // project's entry in the book holds theirs.
        let held = Self::holding(&known);
        let mut reserved = Reserved::default();
        for (run, machine) in Self::placed_runs(&known) {
            self.counted.insert(run);
            reserved.hold(machine);
        }
        let ticket = self.ticket;
        // A project stopped since the pass checked its lease: nothing more is changed.
        let Some(others) = world.book.begin(ticket, held) else {
            return Err(Ended::Lost);
        };
        for machine in &others {
            reserved.hold(*machine);
        }
        self.others = others;

        // 3 and 4. The claims on issues in `triage` and `building` that no run of the ledger
        // holds: Foundry's are carried on, another factory's are foreign.
        let mut carry = Vec::new();
        for state in [State::Triage, State::Building] {
            let listing = world.tracker.list_issues(&repo, state)?;
            for issue in listing.items {
                self.titles.insert(issue.number, issue.title.clone());
                let ours = known.iter().any(|k| {
                    k.info.id.issue() == issue.number && (k.info.confirmed || k.info.unreadable)
                });
                if ours {
                    continue;
                }
                match self.found_claim(&issue)? {
                    Found::Ours(run) => carry.push(run),
                    Found::Foreign => self.foreign(&issue, state),
                    Found::Nothing => {}
                }
            }
        }
        let carried = !carry.is_empty();
        for run in carry {
            let ctx = self.ctx(self.config);
            let said = match start::advance(&ctx, &run) {
                Ok(step) => format!("carried on from its claim: {step:?}"),
                Err(error) => ending(error)?,
            };
            self.note(&run, said);
        }
        if carried {
            // A run carried on holds a slot and its machine from here, though no slot was
            // taken for it: the claims below must read it.
            self.recount(&mut reserved)?;
        }

        // 5. Every run, then the feedback steps, before the claims as the issue orders them.
        let mut later = Vec::new();
        for k in &known {
            let run = &k.info.id;
            if k.info.unreadable {
                self.note(
                    run,
                    "its events cannot be read: nothing was done and it holds a slot",
                );
                continue;
            }
            if !k.info.confirmed {
                continue;
            }
            let pending = feedback::round_pending(&k.events);
            if pending || k.info.state == Some(State::PrOpen) {
                later.push(k);
                continue;
            }
            let said = match self.work_run(k, &mut reserved) {
                Ok(said) => said,
                Err(error) => ending(error)?,
            };
            self.note(run, said);
            if feedback::round_owns(&k.events) {
                later.push(k);
            }
        }

        for k in later {
            let said = match self.feedback(k, &mut reserved) {
                Ok(said) => said,
                Err(error) => ending(error)?,
            };
            self.note(&k.info.id, said);
        }

        // 6. The claims.
        self.claims(&known, &mut reserved)?;

        // 7. The sweeps: orphans for a machine heard from again, and finished runs.
        self.sweep()?;

        // 8. What the status says. Nothing is written after its read of the ledger, so the book
        // is set from the same read.
        let after = self.results_of()?;
        world.book.finish(ticket, Self::holding(&after));
        Ok(())
    }

    fn ticket(&self) -> Ticket {
        self.ticket
    }

    /// Whose claim `issue` carries: a run of Foundry's, named by Foundry's newest claim
    /// comment or, with no claim comment at all, by the fleet's workspace names; another
    /// factory's, or another login's comment is none; or nothing at all.
    fn found_claim(&mut self, issue: &IssueSummary) -> Result<Found, Ended> {
        let world = self.world;
        let repo = &world.project.repo;
        let comments = world.tracker.issue_comments(repo, issue.number)?;
        match newest_claim(&comments, &world.project.login) {
            Some(Claim::Foundry { run, .. })
                if run.repo().eq_ignore_ascii_case(repo) && run.issue() == issue.number =>
            {
                Ok(Found::Ours(run))
            }
            Some(_) => Ok(Found::Foreign),
            None => {
                let next = self.ledger.next_attempt(repo, issue.number)?;
                for attempt in (1..=next).rev() {
                    let name = format!("fac/{}-{attempt}", issue.number);
                    if world
                        .view
                        .workspace_named(world.project.id, &name)
                        .is_some()
                        && let Ok(run) = RunId::new(repo, issue.number, attempt)
                    {
                        return Ok(Found::Ours(run));
                    }
                }
                Ok(Found::Nothing)
            }
        }
    }

    fn foreign(&mut self, issue: &IssueSummary, state: State) {
        let claim = ForeignClaim {
            issue: issue.number,
            title: issue.title.clone(),
            state: state
                .label()
                .strip_prefix("factory:")
                .unwrap_or(state.label())
                .to_owned(),
        };
        self.report.foreign.push(claim.clone());
        self.results.foreign.push(claim);
    }

    /// One run through its steps, routed as each step's module says.
    fn work_run(&mut self, k: &Known, reserved: &mut Reserved) -> Result<String, RunError> {
        let world = self.world;
        let run = &k.info.id;
        let state = k.info.state;
        let config = run_config(self.config, k.info.base.as_deref());
        let ctx = context(self.world, self.ledger, self.settings, &config);
        if gating::gate_running(&k.events) {
            // Its slot is already counted: a gate in flight is polled, not placed.
            return self.gate(&ctx, run, false);
        }
        if k.seen.stop_requested && matches!(state, Some(State::Building | State::Stuck)) {
            match stop::advance(&ctx, run)? {
                StopStep::Skipped { .. } => {}
                StopStep::GateHolds => return self.gate(&ctx, run, true),
                StopStep::Stuck { .. } => return Ok("stuck: stopped by hand".into()),
                other => return Ok(format!("stop: {other:?}")),
            }
        }
        if matches!(state, None | Some(State::Triage | State::Building))
            && let LimitStep::Stopped { reason } = building::time_limit(&ctx, run)?
        {
            return Ok(format!("stuck: {reason}"));
        }
        let gate_holds = k.info.pr_reported || k.seen.pass.is_some();
        if !gate_holds
            && matches!(
                state,
                None | Some(State::Triage | State::Building | State::Stuck)
            )
        {
            match resume::advance(&ctx, run)? {
                ResumeStep::Skipped { .. } | ResumeStep::Moved { .. } => {}
                ResumeStep::NeedsMachine(_) => {
                    return match placing::place_run(
                        world.view,
                        world.shared,
                        &world.project.login,
                        reserved,
                    ) {
                        Ok(machine) => {
                            // Its machine is reserved now, whatever the ledger shows of it later.
                            self.counted.insert(run.clone());
                            Ok(format!("resume: {:?}", resume::placed(&ctx, run, machine)?))
                        }
                        Err(reasons) => Ok(format!(
                            "the resume waits for a machine: {}",
                            reasons.join("; ")
                        )),
                    };
                }
                other => return Ok(format!("resume: {other:?}")),
            }
        }
        if !matches!(state, None | Some(State::Triage | State::NeedsInfo)) {
            match stop::reconcile(&ctx, run)? {
                ReconcileStep::Unchanged
                | ReconcileStep::Skipped { .. }
                | ReconcileStep::Archived { .. } => {}
                // A run whose end is being wound up is touched by no other step.
                other => return Ok(format!("reconcile: {other:?}")),
            }
        }
        match state {
            None | Some(State::Triage) => {
                if feedback::round_owns(&k.events) {
                    return Ok("the feedback round carries it".into());
                }
                Ok(format!("start: {:?}", start::advance(&ctx, run)?))
            }
            Some(State::Building) => {
                let step = building::advance(&ctx, run)?;
                let mut said = format!("build: {step:?}");
                if let BuildStep::ReadyForGate { .. } = step {
                    said = format!("{said}; {}", self.gate(&ctx, run, true)?);
                }
                let forwarded = feedback::forward(&ctx, run)?;
                if forwarded.comments > 0 || forwarded.body_edit {
                    said = format!("{said}; forwarded {forwarded:?}");
                }
                Ok(said)
            }
            Some(State::NeedsInfo | State::Stuck) => {
                Ok(format!("build: {:?}", building::advance(&ctx, run)?))
            }
            Some(other) => Ok(format!("left in {}", other.label())),
        }
    }

    /// The gate step of `run`, with what it says.
    /// A gate slot of the fleet is taken for the step, and given back when it placed no gate.
    fn gate(
        &mut self,
        ctx: &RunContext<'_>,
        run: &RunId,
        may_place: bool,
    ) -> Result<String, RunError> {
        let book = self.world.book;
        let ticket = self.ticket();
        let taken = may_place && book.take_gate(ticket, self.settings.fleet.max_gates);
        let mut slots = Slots {
            runs_free: 0,
            gates_free: u32::from(taken),
        };
        let step = gating::advance(ctx, run, self.world.shared, &mut slots);
        if taken && slots.gates_free > 0 {
            book.give_gate(ticket);
        }
        let step = step?;
        if let GateStep::BackingOff { .. } = step {
            self.results.gate_infra.push(run.clone());
        }
        Ok(format!("gate: {step:?}"))
    }

    /// The feedback step of a run in `pr-open`, or one a round owns.
    /// A run slot of the fleet is taken only once feedback is found and a round can start, and
    /// given back when no round took it.
    fn feedback(&mut self, k: &Known, reserved: &mut Reserved) -> Result<String, RunError> {
        let run = &k.info.id;
        let config = run_config(self.config, k.info.base.as_deref());
        let ctx = context(self.world, self.ledger, self.settings, &config);
        if k.info.state == Some(State::PrOpen) {
            match stop::reconcile(&ctx, run)? {
                ReconcileStep::Unchanged
                | ReconcileStep::Skipped { .. }
                | ReconcileStep::Archived { .. } => {}
                other => return Ok(format!("reconcile: {other:?}")),
            }
        }
        let book = self.world.book;
        let ticket = self.ticket();
        // First with no slot: the read of GitHub, when one is due, then holds none of the
        // fleet's, so another project's claim is not held up by a slow `gh`. A read that finds
        // nothing starts no round and is done. One that found feedback and had no slot to start
        // its round says `no run slot is free` (and records what it found, so the next read
        // goes to that round): only then is a slot taken, if one is free, and the step run once
        // more. Only a run in `pr-open` can start a round; one under way holds its slot already.
        let mut none = Slots {
            runs_free: 0,
            gates_free: 0,
        };
        let mut step = feedback::advance(&ctx, run, self.world.shared, &mut none, reserved)?;
        if k.info.state == Some(State::PrOpen)
            && matches!(&step, FeedbackStep::Waiting { reason } if reason == feedback::NO_SLOT)
            && book.take_run(ticket, self.settings.fleet.max_runs)
        {
            let mut slots = Slots {
                runs_free: 1,
                gates_free: 0,
            };
            let again = feedback::advance(&ctx, run, self.world.shared, &mut slots, reserved);
            if slots.runs_free > 0 {
                book.give_run(ticket);
            }
            step = again?;
        }
        if let FeedbackStep::Waiting { reason } = &step {
            let waiting = Waiting {
                issue: run.issue(),
                title: self.title(run.issue()),
                reason: reason.clone(),
                run: Some(run.clone()),
            };
            self.results.feedback_waiting.push(waiting);
        }
        Ok(format!("feedback: {step:?}"))
    }

    /// Step 6: ready issues claimed, oldest first, while a slot is free and placement has a
    /// machine.
    fn claims(&mut self, known: &[Known], reserved: &mut Reserved) -> Result<(), Ended> {
        let book = self.world.book;
        let ticket = self.ticket();
        let max = self.settings.fleet.max_runs;
        let world = self.world;
        let repo = world.project.repo.clone();
        let listing = world.tracker.list_issues(&repo, State::Ready)?;
        let mut issues = listing.items;
        issues.sort_by_key(|issue| issue.number);
        issues.dedup_by_key(|issue| issue.number);
        let paused = start::claims_paused(self.settings);
        let mut no_machine: Option<String> = None;
        for issue in issues {
            let n = issue.number;
            self.titles.insert(n, issue.title.clone());
            let of_issue: Vec<&Known> = known.iter().filter(|k| k.info.id.issue() == n).collect();
            let wait = |pass: &mut Self, reason: String| {
                let waiting = Waiting {
                    issue: n,
                    title: issue.title.clone(),
                    reason,
                    run: None,
                };
                pass.report.waiting.push(waiting.clone());
                pass.results.ready.push(waiting);
            };
            if of_issue.iter().any(|k| gating::gate_running(&k.events)) {
                wait(self, GATE_RUNNING.into());
                continue;
            }
            // Its events cannot be read, so the claim would end every pass at them: the issue
            // waits, and the run is not counted as a slot freed for it.
            if of_issue.iter().any(|k| k.info.unreadable) {
                wait(self, UNREADABLE_RUN.into());
                continue;
            }
            if let Some(pause) = paused {
                wait(self, pause.reason().into());
                continue;
            }
            if issue
                .labels
                .iter()
                .any(|l| l.eq_ignore_ascii_case(POSTPONED))
            {
                wait(self, "labelled postponed, so it is not claimed".into());
                continue;
            }
            if let Some(reason) = &no_machine {
                wait(self, reason.clone());
                continue;
            }
            // The issue's own triage runs that have no session end with this claim, and leave
            // their slot to it.
            let freed = of_issue
                .iter()
                .filter(|k| {
                    holds_slot(&k.info)
                        && matches!(k.info.state, None | Some(State::Triage))
                        && !k.info.session
                })
                .count();
            // Read before the blockers, so an issue the limit holds back costs no read.
            if freed == 0 && book.runs_free(max) == 0 {
                wait(self, NO_SLOT.into());
                continue;
            }
            let linked = world.tracker.open_blockers(&repo, n)?;
            let blocked = if linked.is_empty() {
                let text = world.tracker.text_blockers(&repo, n)?;
                (!text.is_empty()).then(|| blocked_by(&repo, &text, FROM_TEXT))
            } else {
                Some(blocked_by(&repo, &linked, ""))
            };
            if let Some(blocked) = blocked {
                wait(self, blocked);
                continue;
            }
            // The slot is taken now, under the book's lock: another project's pass may have
            // taken the last one since it was read.
            let taken = freed == 0;
            if taken && !book.take_run(ticket, max) {
                wait(self, NO_SLOT.into());
                continue;
            }
            let give_back = |book: &Book| {
                if taken {
                    book.give_run(ticket);
                }
            };
            let before = reserved.clone();
            let machine = match placing::place_run(
                world.view,
                world.shared,
                &world.project.login,
                reserved,
            ) {
                Ok(machine) => machine,
                Err(reasons) => {
                    give_back(book);
                    let reason = format!("no machine can take it now: {}", reasons.join("; "));
                    no_machine = Some(reason.clone());
                    wait(self, reason);
                    continue;
                }
            };
            let ctx = self.ctx(self.config);
            let outcome = match start::claim(&ctx, n, machine) {
                Ok(outcome) => outcome,
                Err(error) => {
                    *reserved = before;
                    give_back(book);
                    // The claim may have recorded its run before it failed: that run holds its
                    // slot and its machine, whatever the error. If the ledger cannot be read,
                    // the pass ends below or the next one counts it again.
                    let _ = self.recount(reserved);
                    let said = ending(error)?;
                    wait(self, said);
                    continue;
                }
            };
            match outcome {
                ClaimOutcome::Claimed { run } => {
                    self.report.claimed.push(run.clone());
                    // Its machine is reserved now, whatever the ledger shows of it later.
                    self.counted.insert(run.clone());
                    book.place(ticket, machine);
                    let ctx = self.ctx(self.config);
                    let said = match start::advance(&ctx, &run) {
                        Ok(step) => match step {
                            StartStep::Started { .. } => "claimed, session started".to_owned(),
                            other => format!("claimed on {machine}: {other:?}"),
                        },
                        Err(error) => format!("claimed on {machine}; {}", ending(error)?),
                    };
                    self.note(&run, said);
                }
                ClaimOutcome::Skipped { reason } => {
                    *reserved = before;
                    give_back(book);
                    wait(self, reason);
                }
                ClaimOutcome::Refused { .. } | ClaimOutcome::Abandoned { .. } => {
                    *reserved = before;
                    give_back(book);
                }
            }
        }
        Ok(())
    }

    /// Step 7: finished runs swept by the reconcile every [`STUCK_READ_EVERY`], and the
    /// orphans of every run archived when a machine is heard from again.
    fn sweep(&mut self) -> Result<(), Ended> {
        let world = self.world;
        let now = world.clock.now();
        let online = world.view_online();
        let heard_again = self.memo.orphans_owed
            || match &self.memo.online {
                None => true,
                Some(before) => online.iter().any(|machine| !before.contains(machine)),
            };
        let sweep_due = self
            .memo
            .swept
            .is_none_or(|at| now.saturating_sub(at) >= STUCK_READ_EVERY);
        if !heard_again && !sweep_due {
            return Ok(());
        }
        if sweep_due {
            if self.memo.sweep_unfinished {
                // The retry of a sweep that ended part way: should it end part way too, the
                // next sweep waits for STUCK_READ_EVERY, so one run cannot fail every pass.
                self.memo.swept = Some(now);
                self.memo.sweep_unfinished = false;
            } else {
                self.memo.sweep_unfinished = true;
            }
        }
        let repo = world.project.repo.clone();
        let runs = world
            .store
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .runs_of(&repo)?;
        let mut owed = false;
        let after = self.memo.sweep_after.clone();
        for run in runs {
            // A sweep that ended part way goes on from the run after the one it stopped at.
            let in_sweep = sweep_due && after.as_ref().is_none_or(|after| run > *after);
            // A run finished is known by the memo, or asked of the store with no row read.
            // The pass reads a run's events itself only for its orphans: the reconcile reads
            // them for its own sake.
            let finished = in_sweep
                && (self.memo.finished.contains_key(&run)
                    || world
                        .store
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .run_has_event(&run, names::FINISHED)?);
            if finished {
                let ctx = self.ctx(self.config);
                match stop::reconcile(&ctx, &run) {
                    Ok(ReconcileStep::Archived { workspaces }) => {
                        self.report.archived.extend(workspaces);
                    }
                    Ok(_) => {}
                    // A finished run whose events this build cannot read (a row that does not
                    // decode) is the run's own. Any other store error ends the pass, as it did.
                    Err(RunError::Store(StoreError::Json(_))) => {
                        self.note(&run, UNREADABLE_SWEPT);
                    }
                    Err(error) => {
                        // Should the error end the pass, the next sweep goes on after this run.
                        self.memo.sweep_after = Some(run.clone());
                        let said = ending(error)?;
                        self.note(&run, said);
                    }
                }
            }
            if !heard_again {
                continue;
            }
            // A run whose events this build cannot read (a newer build's) is noted, and the
            // sweep goes on: it must not end every pass of the project.
            let events = match self.ledger.read(&run) {
                Ok(events) => events,
                Err(StoreError::Json(_)) => {
                    self.note(&run, UNREADABLE_SWEPT);
                    continue;
                }
                // The store failed for a reason that is not the build's: the pass ends, and the
                // next one sweeps again.
                Err(error) => return Err(error.into()),
            };
            for workspace in resume::orphans(world.view, world.project.id, &run, &events) {
                if self.report.archived.contains(&workspace) {
                    continue;
                }
                if world.guard.check().is_err() {
                    return Err(Ended::Lost);
                }
                match world.worker.archive(workspace) {
                    Ok(()) => self.report.archived.push(workspace),
                    Err(error) => {
                        owed = true;
                        self.note(&run, format!("an orphan was not archived: {error}"));
                    }
                }
            }
        }
        // Only a sweep that went through every run is done: one that ended part way is due
        // again at the next pass, once.
        if sweep_due {
            self.memo.swept = Some(now);
            self.memo.sweep_unfinished = false;
            self.memo.sweep_after = None;
        }
        if heard_again {
            self.memo.orphans_owed = owed;
        }
        Ok(())
    }

    /// The live runs, the held-up runs and the finished runs, for the status.
    /// Answers the unfinished runs it read.
    fn results_of(&mut self) -> Result<Vec<Known>, Ended> {
        let world = self.world;
        let known = self.known()?;
        let unfinished: BTreeSet<RunId> = known.iter().map(|k| k.info.id.clone()).collect();
        for k in &known {
            let run = k.info.id.clone();
            match k.info.state {
                Some(State::Stuck | State::NeedsInfo) => {
                    let kind = if k.info.state == Some(State::Stuck) {
                        HeldUpKind::Stuck
                    } else {
                        HeldUpKind::NeedsInfo
                    };
                    self.results.held_up.push(HeldUp {
                        run,
                        kind,
                        reason: k.seen.stuck.clone().unwrap_or_default(),
                    });
                }
                _ if k.info.unreadable || !k.info.confirmed => {}
                state => {
                    let machine = k
                        .seen
                        .machine
                        .as_deref()
                        .and_then(machine_id)
                        .unwrap_or(world.me);
                    let used = super::runs::progress::next_gate_attempt(&k.seen).saturating_sub(1);
                    self.results.live(LiveRun {
                        title: self.title(run.issue()),
                        step: state
                            .map_or("claimed", |s| {
                                s.label().strip_prefix("factory:").unwrap_or(s.label())
                            })
                            .to_owned(),
                        machine,
                        gate_retries_used: used,
                        minutes_used: k.seen.worked_ms / 60_000,
                        workspaces: k
                            .seen
                            .workspace
                            .as_deref()
                            .and_then(|w| w.parse().ok().map(WorkspaceId))
                            .into_iter()
                            .collect(),
                        sessions: k
                            .seen
                            .thread_id
                            .as_deref()
                            .and_then(|s| s.parse().ok().map(SessionId))
                            .into_iter()
                            .collect(),
                        run,
                    });
                }
            }
        }
        self.see_finished(&unfinished)?;
        // A run that finished stuck or needs-info waits for a person while its issue keeps that
        // label, when it is the issue's newest run. The label is read only when such a run is.
        let mut newest: BTreeMap<u64, &RunId> = BTreeMap::new();
        for run in self.memo.finished.keys().chain(&unfinished) {
            let slot = newest.entry(run.issue()).or_insert(run);
            if run.attempt() > slot.attempt() {
                *slot = run;
            }
        }
        let waiting: Vec<(&Done, HeldUpKind)> = newest
            .values()
            .filter_map(|run| {
                let done = self.memo.finished.get(*run)?.as_ref()?;
                match done.run.outcome {
                    RunOutcome::Stuck => Some((done, HeldUpKind::Stuck)),
                    RunOutcome::NeedsInfo => Some((done, HeldUpKind::NeedsInfo)),
                    RunOutcome::PrOpen => None,
                }
            })
            .collect();
        for (kind, state) in [
            (HeldUpKind::Stuck, State::Stuck),
            (HeldUpKind::NeedsInfo, State::NeedsInfo),
        ] {
            let mut of_kind = waiting.iter().filter(|(_, k)| *k == kind).peekable();
            if of_kind.peek().is_none() {
                continue;
            }
            let labelled: BTreeSet<u64> = world
                .tracker
                .list_issues(&world.project.repo, state)?
                .items
                .iter()
                .map(|issue| issue.number)
                .collect();
            for (done, _) in of_kind {
                if labelled.contains(&done.run.run.issue()) {
                    self.results.held_up.push(HeldUp {
                        run: done.run.run.clone(),
                        kind,
                        reason: done.reason.clone().unwrap_or_default(),
                    });
                }
            }
        }
        let mut done: Vec<&Done> = self.memo.finished.values().flatten().collect();
        // Newest first by where the `finished` event stands, then by run id for a tie.
        done.sort_by(|a, b| (&b.at, &b.run.run).cmp(&(&a.at, &a.run.run)));
        self.results.finished = done.into_iter().map(|done| done.run.clone()).collect();
        Ok(known)
    }

    /// Reads into the memo each run that is not in `unfinished` and not seen finished yet. A
    /// finished run's summary never changes, so a pass over a history it has seen reads no
    /// finished run's events.
    fn see_finished(&mut self, unfinished: &BTreeSet<RunId>) -> Result<(), Ended> {
        let world = self.world;
        let repo = &world.project.repo;
        let unseen: BTreeSet<RunId> = world
            .store
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .runs_of(repo)?
            .into_iter()
            .filter(|id| !unfinished.contains(id) && !self.memo.finished.contains_key(id))
            .collect();
        if unseen.is_empty() {
            return Ok(());
        }
        for summary in self.ledger.run_summaries(&unseen) {
            if summary.unreadable {
                // A finished run a newer build wrote: not shown, and not read again.
                self.memo.finished.insert(summary.id, None);
                continue;
            }
            if !summary.finished {
                continue;
            }
            let at = finishing(
                &world.store.lock().unwrap_or_else(PoisonError::into_inner),
                &summary.id,
            );
            let id = summary.id.clone();
            let reason = summary.reason.clone();
            let run = summary.state.map(|state| FinishedRun {
                title: summary.title.clone().unwrap_or_default(),
                outcome: match state {
                    State::Stuck => RunOutcome::Stuck,
                    State::NeedsInfo => RunOutcome::NeedsInfo,
                    _ => RunOutcome::PrOpen,
                },
                pr_end: summary.pr_end,
                pr_url: summary.pr_url.clone(),
                run: summary.id,
            });
            self.memo
                .finished
                .insert(id, run.map(|run| Done { at, run, reason }));
        }
        Ok(())
    }
}

/// Where the newest `finished` event of `run` stands: its lease generation, its record's
/// version and its writer. A writer's versions only rise, and a later lease holder writes at a
/// higher generation, so this orders every machine's finished runs, which no clock does.
fn finishing(store: &Store, run: &RunId) -> Option<(u64, u64, MachineId)> {
    let mut newest = None;
    for writer in store.run_writers(run).ok()? {
        for (version, event) in store.run_events_by(run, writer, None).ok()? {
            if event.event == names::FINISHED {
                newest = newest.max(Some((event.generation, version, writer)));
            }
        }
    }
    newest
}

impl PassResults {
    fn live(&mut self, run: LiveRun) {
        self.runs.push(run);
    }
}

/// A machine id as the ledger writes one.
fn machine_id(text: &str) -> Option<MachineId> {
    uuid::Uuid::parse_str(text).ok().map(MachineId)
}

/// What a blocker the `## Depends on` text names carries after it.
const FROM_TEXT: &str = " (from Depends on, add the GitHub link)";

/// "blocked by #2, other/repo#3", each blocker followed by `suffix`, as the runner says it.
fn blocked_by(repo: &str, blockers: &[super::github::Blocker], suffix: &str) -> String {
    let named: Vec<String> = blockers
        .iter()
        .map(|blocker| {
            if blocker.repo.eq_ignore_ascii_case(repo) {
                format!("#{}{suffix}", blocker.number)
            } else {
                format!("{}#{}{suffix}", blocker.repo, blocker.number)
            }
        })
        .collect();
    format!("blocked by {}", named.join(", "))
}

/// GitHub in memory for the pass's tests and the service's: many issues of one repo.
#[cfg(test)]
pub(crate) mod testing {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};

    use serde_json::{Value, json};

    use crate::factory::github::{OtherKind, Tracker, TrackerError};
    use crate::factory::runs::testing::approval_answer;

    pub(crate) const REPO: &str = "rawphp/softfac";
    pub(crate) const LOGIN: &str = "softfac-bot";
    pub(crate) const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    pub(crate) const LABELED_AT: &str = "2026-10-04T10:00:00Z";
    pub(crate) const CONFIG: &str = "factory:\n  instructors: [tom]\n  base: main\n  provision: []\n  gate: [cargo test]\n  autonomy: pr\n  limits: { minutes: 90, gate_retries: 2, checks_minutes: 30, interruption_resumes: 2 }\n";

    // ----- GitHub in memory, many issues -----

    #[derive(Clone, Debug)]
    pub(crate) struct HubIssue {
        pub(crate) title: String,
        pub(crate) body: String,
        pub(crate) labels: Vec<String>,
        pub(crate) assignees: Vec<String>,
        pub(crate) closed: bool,
        /// Open issues a dependency link says block it.
        pub(crate) linked: Vec<u64>,
        /// Open issues of other repos a dependency link says block it.
        pub(crate) linked_in: Vec<(String, u64)>,
    }

    #[derive(Default)]
    pub(crate) struct HubState {
        pub(crate) issues: BTreeMap<u64, HubIssue>,
        /// `(id, issue, author, body)`.
        pub(crate) comments: Vec<(u64, u64, String, String)>,
        pub(crate) next_comment: u64,
        pub(crate) head: String,
        pub(crate) config: String,
        /// A needle and how many calls holding it fail.
        pub(crate) failures: Vec<(String, usize)>,
        pub(crate) calls: Vec<Vec<String>>,
        /// Writes made after the guard first failed.
        pub(crate) late_writes: Vec<String>,
        /// Open draft pull requests by number, with their head branch.
        pub(crate) pulls: BTreeMap<u64, String>,
    }

    /// GitHub as one login sees one repo, every issue kept, every call recorded.
    pub(crate) struct Hub {
        pub(crate) state: Mutex<HubState>,
        /// Set once the pass's guard has failed: a write after it is recorded as late.
        pub(crate) lost: Arc<AtomicBool>,
    }

    pub(crate) fn b64(bytes: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let n = chunk.len();
            let word = (u32::from(chunk[0]) << 16)
                | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
                | u32::from(*chunk.get(2).unwrap_or(&0));
            for at in 0..4 {
                if at <= n {
                    out.push(TABLE[((word >> (18 - 6 * at)) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    impl Hub {
        pub(crate) fn new() -> Arc<Self> {
            Arc::new(Self {
                state: Mutex::new(HubState {
                    next_comment: 100,
                    head: SHA.into(),
                    config: CONFIG.into(),
                    ..HubState::default()
                }),
                lost: Arc::new(AtomicBool::new(false)),
            })
        }

        pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, HubState> {
            self.state.lock().unwrap_or_else(PoisonError::into_inner)
        }

        /// An open issue with `labels`, nobody assigned.
        pub(crate) fn issue(&self, number: u64, labels: &[&str]) -> &Self {
            self.lock().issues.insert(
                number,
                HubIssue {
                    title: format!("Issue {number}"),
                    body: "Do the thing.".into(),
                    labels: labels.iter().map(|l| (*l).to_owned()).collect(),
                    assignees: vec![],
                    closed: false,
                    linked: vec![],
                    linked_in: vec![],
                },
            );
            self
        }

        pub(crate) fn ready(&self, number: u64) -> &Self {
            self.issue(number, &["factory:ready"])
        }

        pub(crate) fn edit(&self, number: u64, change: impl FnOnce(&mut HubIssue)) {
            change(self.lock().issues.get_mut(&number).expect("an issue"));
        }

        pub(crate) fn comment_as(&self, issue: u64, author: &str, body: &str) {
            let mut state = self.lock();
            let id = state.next_comment;
            state.next_comment += 1;
            state.comments.push((id, issue, author.into(), body.into()));
        }

        pub(crate) fn fail(&self, needle: &str, times: usize) {
            self.lock().failures.push((needle.into(), times));
        }

        pub(crate) fn calls(&self) -> Vec<Vec<String>> {
            self.lock().calls.clone()
        }

        /// The calls that change GitHub.
        pub(crate) fn writes(&self) -> Vec<String> {
            self.calls()
                .into_iter()
                .filter(|args| is_write(args))
                .map(|args| args.join(" "))
                .collect()
        }

        pub(crate) fn labels(&self, number: u64) -> Vec<String> {
            self.lock().issues[&number].labels.clone()
        }

        pub(crate) fn late_writes(&self) -> Vec<String> {
            self.lock().late_writes.clone()
        }

        pub(crate) fn other(args: &[String], message: &str) -> TrackerError {
            TrackerError::Other {
                kind: OtherKind::Failed,
                message: format!("`gh {}` failed: {message}", args.join(" ")),
            }
        }

        pub(crate) fn flag_values(args: &[String], flag: &str) -> Vec<String> {
            args.windows(2)
                .filter(|pair| pair[0] == flag)
                .map(|pair| pair[1].clone())
                .collect()
        }

        pub(crate) fn number_arg(args: &[String]) -> Option<u64> {
            Self::flag_values(args, "-F")
                .into_iter()
                .find_map(|f| f.strip_prefix("number=").and_then(|n| n.parse().ok()))
        }

        pub(crate) fn issue_json(number: u64, issue: &HubIssue) -> Value {
            json!({
                "number": number,
                "title": issue.title,
                "body": issue.body,
                "labels": issue.labels.iter().map(|n| json!({"name": n})).collect::<Vec<_>>(),
                "assignees": issue.assignees.iter().map(|n| json!({"login": n})).collect::<Vec<_>>(),
                "state": if issue.closed { "CLOSED" } else { "OPEN" },
                "author": {"login": "tom"},
            })
        }

        pub(crate) fn answer(&self, args: &[String]) -> Result<String, TrackerError> {
            let mut state = self.lock();
            state.calls.push(args.to_vec());
            if is_write(args) && self.lost.load(Ordering::SeqCst) {
                state.late_writes.push(args.join(" "));
            }
            let joined = args.join(" ");
            if let Some(at) = state
                .failures
                .iter()
                .position(|(needle, times)| *times > 0 && joined.contains(needle.as_str()))
            {
                state.failures[at].1 -= 1;
                return Err(Self::other(args, "injected failure"));
            }
            let word = |at: usize| args.get(at).map(String::as_str);
            let number = |at: usize| word(at).and_then(|n| n.parse::<u64>().ok());
            match (word(0), word(1)) {
                (Some("issue"), Some("list")) => {
                    let label = Self::flag_values(args, "--label").pop().unwrap_or_default();
                    let rows: Vec<Value> = state
                        .issues
                        .iter()
                        .filter(|(_, issue)| !issue.closed && issue.labels.contains(&label))
                        .map(|(n, issue)| Self::issue_json(*n, issue))
                        .collect();
                    Ok(Value::Array(rows).to_string())
                }
                (Some("issue"), Some("view")) => {
                    let n = number(2).unwrap_or(0);
                    let issue = state
                        .issues
                        .get(&n)
                        .ok_or_else(|| Self::other(args, "no such issue"))?;
                    Ok(Self::issue_json(n, issue).to_string())
                }
                (Some("issue"), Some("edit")) => {
                    let n = number(2).unwrap_or(0);
                    let issue = state
                        .issues
                        .get_mut(&n)
                        .ok_or_else(|| Self::other(args, "no such issue"))?;
                    for label in Self::flag_values(args, "--add-label") {
                        if !issue.labels.contains(&label) {
                            issue.labels.push(label);
                        }
                    }
                    for label in Self::flag_values(args, "--remove-label") {
                        issue.labels.retain(|l| *l != label);
                    }
                    for login in Self::flag_values(args, "--add-assignee") {
                        if !issue.assignees.contains(&login) {
                            issue.assignees.push(login);
                        }
                    }
                    for login in Self::flag_values(args, "--remove-assignee") {
                        issue.assignees.retain(|l| *l != login);
                    }
                    Ok(String::new())
                }
                (Some("issue"), Some("comment")) => {
                    let n = number(2).unwrap_or(0);
                    let body = Self::flag_values(args, "--body").pop().unwrap_or_default();
                    let id = state.next_comment;
                    state.next_comment += 1;
                    state.comments.push((id, n, LOGIN.into(), body));
                    Ok(String::new())
                }
                (Some("api"), Some("graphql")) if joined.contains("closedByPullRequestsReferences") => {
                    Ok(json!({"data": {"repository": {"issue": {"closedByPullRequestsReferences": {"nodes": []}}}}}).to_string())
                }
                (Some("api"), Some("graphql")) => {
                    let _ = Self::number_arg(args);
                    Ok(approval_answer(LABELED_AT, "tom", &[]))
                }
                (Some("api"), Some("-X")) => {
                    let path = word(3).unwrap_or_default();
                    let id: u64 = path.rsplit('/').next().and_then(|id| id.parse().ok()).unwrap_or(0);
                    let body = Self::flag_values(args, "-f")
                        .pop()
                        .and_then(|f| f.strip_prefix("body=").map(str::to_owned))
                        .unwrap_or_default();
                    if let Some(row) = state.comments.iter_mut().find(|row| row.0 == id) {
                        row.3 = body;
                    }
                    Ok(String::new())
                }
                (Some("api"), Some(path)) if path.starts_with("repos/") && !path.starts_with(&format!("repos/{REPO}")) => {
                    // An issue of another repo, named by a Depends on: open.
                    Ok(json!({"state": "open"}).to_string())
                }
                (Some("api"), Some(path)) => {
                    let rest = path.strip_prefix(&format!("repos/{REPO}")).unwrap_or(path);
                    if rest.is_empty() {
                        return Ok(json!({"default_branch": "main"}).to_string());
                    }
                    if rest == "/git/ref/heads/main" {
                        return Ok(json!({"object": {"sha": state.head, "type": "commit"}}).to_string());
                    }
                    if let Some(query) = rest.strip_prefix("/contents/.dw/config.yml?ref=") {
                        if query != state.head {
                            return Err(Self::other(args, "no such commit"));
                        }
                        let bytes = state.config.as_bytes();
                        return Ok(json!({"type": "file", "size": bytes.len(), "encoding": "base64", "content": b64(bytes)}).to_string());
                    }
                    let parts: Vec<&str> = rest.trim_start_matches('/').split('/').collect();
                    match parts.as_slice() {
                        ["issues", n, comments] if comments.starts_with("comments") => {
                            let n: u64 = n.parse().unwrap_or(0);
                            let rows: Vec<Value> = state
                                .comments
                                .iter()
                                .filter(|row| row.1 == n)
                                .map(|(id, _, author, body)| json!({"id": id, "user": {"login": author}, "body": body}))
                                .collect();
                            Ok(Value::Array(rows).to_string())
                        }
                        ["issues", n, "dependencies", "blocked_by"] => {
                            let n: u64 = n.parse().unwrap_or(0);
                            let issue = &state.issues[&n];
                            let rows: Vec<Value> = issue
                                .linked
                                .iter()
                                .map(|b| (REPO.to_owned(), *b))
                                .chain(issue.linked_in.iter().cloned())
                                .map(|(repo, b)| json!({"number": b, "state": "open", "repository_url": format!("https://api.github.com/repos/{repo}")}))
                                .collect();
                            Ok(Value::Array(rows).to_string())
                        }
                        ["issues", n] => {
                            let n: u64 = n.parse().unwrap_or(0);
                            let open = state.issues.get(&n).is_some_and(|issue| !issue.closed);
                            Ok(json!({"state": if open { "open" } else { "closed" }}).to_string())
                        }
                        _ => Err(Self::other(args, "the hub does not know this path")),
                    }
                }
                (Some("pr"), Some("list")) => Ok("[]".into()),
                (Some("pr"), Some("view")) => {
                    let n = number(2).unwrap_or(0);
                    let head = state
                        .pulls
                        .get(&n)
                        .ok_or_else(|| Self::other(args, "Could not resolve to a PullRequest"))?;
                    Ok(json!({
                        "number": n,
                        "isDraft": true,
                        "state": "OPEN",
                        "headRefName": head,
                        "isCrossRepository": false,
                        "baseRefName": "main",
                        "headRefOid": SHA,
                        "mergeable": "MERGEABLE",
                        "createdAt": LABELED_AT,
                    })
                    .to_string())
                }
                _ => Err(Self::other(args, "the hub does not know this call")),
            }
        }
    }

    /// Whether `args` change GitHub.
    pub(crate) fn is_write(args: &[String]) -> bool {
        let word = |at: usize| args.get(at).map(String::as_str);
        matches!(
            (word(0), word(1)),
            (Some("issue"), Some("edit" | "comment" | "close"))
                | (Some("pr"), Some("ready" | "comment" | "edit"))
                | (Some("api"), Some("-X"))
                | (Some("label"), Some("create"))
        )
    }

    impl Tracker for Hub {
        fn gh(&self, args: &[String]) -> Result<String, TrackerError> {
            self.answer(args)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use foundry_proto::factory::{
        FactorySettings, FactoryStatus, MachineHeld, NotHolding, ProjectFactory, RunId, SessionTurn,
    };
    use foundry_proto::ids::{MachineId, ProjectId, SessionId, TerminalId, WorkspaceId};
    use foundry_proto::record::{MachineSample, Record, Session, Workspace};
    use foundry_proto::settings::{Limits, Machine, SharedSettings};
    use foundry_proto::state::SessionState;

    use super::*;
    use crate::clock::FakeClock;
    use crate::factory::github::Hiding;
    use crate::factory::lease::LeaseState;
    use crate::factory::ledger::Ledger;
    use crate::factory::ledger::events::{Cause, CauseStep, Event};
    use crate::factory::ledger::testing::open_store;
    use crate::factory::messages::Message;
    use crate::factory::runs::RunProject;
    use crate::factory::worker::{
        AlwaysHeld, Call, CreateWorkspace, FakeWorker, FleetView, Guard, LeaseLost, Lost, Worker,
        WorkerError,
    };
    use crate::sessions::factory::{FactoryStart, SendRefused};
    use crate::sessions::turns::TurnText;
    use crate::store::Store;

    use super::testing::{CONFIG, Hub, LABELED_AT, LOGIN, REPO, SHA};
    const SHA2: &str = "89abcdef0123456789abcdef0123456789abcdef";

    fn me() -> MachineId {
        MachineId(uuid::Uuid::from_u128(0x1))
    }

    fn builder_machine() -> MachineId {
        MachineId(uuid::Uuid::from_u128(0x2))
    }

    fn project_id() -> ProjectId {
        ProjectId(uuid::Uuid::from_u128(0x7001))
    }

    fn other_project() -> ProjectId {
        ProjectId(uuid::Uuid::from_u128(0x7002))
    }

    // ----- a guard that is lost after a number of checks -----

    /// Holds the lease for `checks` checks, then loses it, and says so to the hub and the
    /// worker, so a change made after it is seen.
    struct HeldFor {
        left: AtomicUsize,
        lost: Arc<AtomicBool>,
    }

    impl Guard for HeldFor {
        fn check(&self) -> Result<u64, LeaseLost> {
            let taken = self
                .left
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                });
            match taken {
                Ok(_) => Ok(1),
                Err(_) => {
                    self.lost.store(true, Ordering::SeqCst);
                    Err(LeaseLost {
                        reason: "the test's guard ran out of checks".into(),
                    })
                }
            }
        }
    }

    /// What a test runs after an archive call, with the workspace archived.
    type ArchiveHook = Box<dyn Fn(WorkspaceId) + Send + Sync>;

    /// The fake worker, with every call made after the guard first failed recorded as late.
    struct Watched {
        inner: FakeWorker,
        lost: Arc<AtomicBool>,
        late: Mutex<Vec<String>>,
        /// Run after each archive call, with the workspace archived.
        after_archive: Mutex<Option<ArchiveHook>>,
    }

    impl Watched {
        fn note(&self, what: &str) {
            if self.lost.load(Ordering::SeqCst) {
                self.late.lock().unwrap().push(what.into());
            }
        }
    }

    impl Worker for Watched {
        fn create(&self, m: MachineId, r: CreateWorkspace) -> Result<WorkspaceId, WorkerError> {
            self.note("create");
            self.inner.create(m, r)
        }
        fn start_session(
            &self,
            w: WorkspaceId,
            s: FactoryStart,
            f: Message,
        ) -> Result<SessionId, WorkerError> {
            self.note("start_session");
            self.inner.start_session(w, s, f)
        }
        fn send(&self, s: SessionId, m: Message, o: Option<u64>) -> Result<u64, SendRefused> {
            self.note("send");
            self.inner.send(s, m, o)
        }
        fn turn(&self, s: SessionId, n: u64) -> Result<TurnText, WorkerError> {
            self.inner.turn(s, n)
        }
        fn tail(&self, t: TerminalId) -> Result<String, WorkerError> {
            self.inner.tail(t)
        }
        fn end_sessions(&self, w: WorkspaceId) -> Result<(), WorkerError> {
            self.note("end_sessions");
            self.inner.end_sessions(w)
        }
        fn archive(&self, w: WorkspaceId) -> Result<(), WorkerError> {
            self.note("archive");
            let archived = self.inner.archive(w);
            if let Some(hook) = self.after_archive.lock().unwrap().as_ref() {
                hook(w);
            }
            archived
        }
    }

    // ----- the rig -----

    struct Rig {
        _dir: tempfile::TempDir,
        store: Arc<Mutex<Store>>,
        ledger: Ledger,
        hub: Arc<Hub>,
        tracker: Hiding,
        worker: Watched,
        view: FleetView,
        guard: Box<dyn Guard>,
        lease: LeaseState,
        clock: FakeClock,
        project: RunProject,
        shared: SharedSettings,
        book: Book,
        memo: Memo,
    }

    fn limits(max_sessions: u32) -> Limits {
        Limits {
            max_sessions,
            max_load_per_core: 1.0,
            min_free_disk: 10_000_000_000,
        }
    }

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
                version: None,
                update_ready: None,
            },
        }
    }

    /// A fleet of this machine and one builder that holds the login and the skill.
    fn fleet() -> FleetView {
        FleetView::builder()
            .online(me())
            .online(builder_machine())
            .sample(sample(builder_machine(), 0))
            .held(
                builder_machine(),
                MachineHeld {
                    logins: vec![LOGIN.into()],
                    skill: true,
                },
            )
            .build()
    }

    impl Rig {
        fn new() -> Self {
            let (dir, store) = open_store();
            // Stamped by the wall clock, which the time limit reads.
            let ledger = Ledger::new(
                store.clone(),
                me(),
                1,
                crate::factory::ledger::testing::shown(),
            );
            let hub = Hub::new();
            let tracker = Hiding::new(hub.clone(), Arc::new(|text: &str| text.to_string()));
            let lost = hub.lost.clone();
            let mut shared = SharedSettings {
                machines: [(me(), "coordinator"), (builder_machine(), "builder")]
                    .into_iter()
                    .map(|(id, name)| Machine {
                        id,
                        name: name.into(),
                        address: String::new(),
                        key: String::new(),
                    })
                    .collect(),
                limits: [(me(), limits(0)), (builder_machine(), limits(8))]
                    .into_iter()
                    .collect(),
                factory: Some(FactorySettings::default()),
                ..SharedSettings::default()
            };
            shared.project_factory.insert(
                project_id(),
                ProjectFactory {
                    on: true,
                    login: LOGIN.into(),
                    paused: false,
                },
            );
            Self {
                _dir: dir,
                store,
                ledger,
                hub,
                tracker,
                worker: Watched {
                    inner: FakeWorker::new(),
                    lost,
                    late: Mutex::new(vec![]),
                    after_archive: Mutex::new(None),
                },
                view: fleet(),
                guard: Box::new(AlwaysHeld(1)),
                lease: LeaseState::Held { generation: 1 },
                clock: FakeClock::new(),
                project: RunProject {
                    id: project_id(),
                    repo: REPO.into(),
                    login: LOGIN.into(),
                },
                shared,
                book: Book::default(),
                memo: Memo::default(),
            }
        }

        fn fleet_settings(&mut self, change: impl FnOnce(&mut FactorySettings)) {
            change(
                self.shared
                    .factory
                    .get_or_insert_with(FactorySettings::default),
            );
        }

        fn project_settings(&mut self, change: impl FnOnce(&mut ProjectFactory)) {
            change(self.shared.project_factory.get_mut(&project_id()).unwrap());
        }

        fn guard_for(&mut self, checks: usize) {
            self.guard = Box::new(HeldFor {
                left: AtomicUsize::new(checks),
                lost: self.hub.lost.clone(),
            });
        }

        fn pass(&mut self) -> PassReport {
            let world = World {
                project: &self.project,
                lease: &self.lease,
                guard: self.guard.as_ref(),
                tracker: &self.tracker,
                worker: &self.worker,
                view: &self.view,
                ledger: &self.ledger,
                store: &self.store,
                me: me(),
                shared: &self.shared,
                book: &self.book,
                clock: &self.clock,
            };
            run(&world, &mut self.memo)
        }

        /// The status this machine wrote for the project.
        fn status(&self) -> FactoryStatus {
            let statuses = self.store.lock().unwrap().statuses(project_id()).unwrap();
            statuses
                .into_iter()
                .find(|(writer, _)| *writer == me())
                .map(|(_, status)| status)
                .expect("a status")
        }

        fn creates(&self) -> Vec<CreateWorkspace> {
            self.worker
                .inner
                .calls()
                .into_iter()
                .filter_map(|call| match call {
                    Call::Create { request, .. } => Some(request),
                    _ => None,
                })
                .collect()
        }

        fn claimed_issues(&self) -> Vec<u64> {
            self.hub
                .writes()
                .iter()
                .filter(|w| w.contains("foundry:claim"))
                .filter_map(|w| w.split_whitespace().nth(2).and_then(|n| n.parse().ok()))
                .collect()
        }

        /// A run of `issue` in the ledger: claimed on the builder and approved, in `state`.
        fn run_in(&self, issue: u64, attempt: u32, state: State) -> RunId {
            let run = RunId::new(REPO, issue, attempt).unwrap();
            let cause = Cause::new(CauseStep::Start, "test");
            self.ledger
                .write(
                    &run,
                    &Event::StepChanged {
                        step: format!("approved {LABELED_AT}"),
                    },
                    &cause,
                )
                .unwrap();
            self.ledger
                .write(
                    &run,
                    &Event::Claimed {
                        machine: builder_machine().to_string(),
                        github_user: LOGIN.into(),
                    },
                    &cause,
                )
                .unwrap();
            if state != State::Ready {
                self.ledger
                    .write(
                        &run,
                        &Event::StateChanged {
                            from: State::Ready,
                            to: State::Triage,
                        },
                        &cause,
                    )
                    .unwrap();
            }
            if !matches!(state, State::Ready | State::Triage) {
                self.ledger
                    .write(
                        &run,
                        &Event::StateChanged {
                            from: State::Triage,
                            to: state,
                        },
                        &cause,
                    )
                    .unwrap();
            }
            run
        }
    }

    fn waiting_reason(report: &PassReport, issue: u64) -> Option<String> {
        report
            .waiting
            .iter()
            .find(|w| w.issue == issue)
            .map(|w| w.reason.clone())
    }

    // ----- the acceptance cases of the plan -----

    mod the_lease {
        use super::*;

        #[test]
        fn a_project_with_no_token_writes_no_token_and_touches_nothing_on_github() {
            let mut rig = Rig::new();
            rig.hub.ready(5);
            rig.lease = LeaseState::NoToken;
            rig.guard = Box::new(Lost);
            let report = rig.pass();
            assert!(rig.hub.calls().is_empty(), "{:?}", rig.hub.calls());
            assert!(rig.worker.inner.calls().is_empty());
            let status = rig.status();
            assert!(!status.holds);
            assert_eq!(
                status.not_holding,
                Some(NotHolding::NoToken {
                    login: LOGIN.into()
                })
            );
            assert_eq!(report.generation, None);
        }

        #[test]
        fn a_project_whose_lease_another_machine_holds_writes_who_holds_it_and_claims_nothing() {
            let mut rig = Rig::new();
            rig.hub.ready(5);
            rig.lease = LeaseState::HeldBy {
                machine: builder_machine(),
                generation: 4,
            };
            rig.guard = Box::new(Lost);
            rig.pass();
            assert!(rig.hub.writes().is_empty(), "{:?}", rig.hub.writes());
            let status = rig.status();
            assert!(!status.holds);
            assert_eq!(
                status.not_holding,
                Some(NotHolding::HeldBy {
                    machine: builder_machine(),
                    generation: 4
                })
            );
        }

        #[test]
        fn a_lease_with_no_known_holder_is_not_holding_with_the_reason_as_idle() {
            let mut rig = Rig::new();
            rig.lease = LeaseState::Unknown {
                reason: "the lease ref could not be read",
            };
            rig.guard = Box::new(Lost);
            rig.pass();
            let status = rig.status();
            assert!(!status.holds);
            assert_eq!(status.not_holding, None);
            assert_eq!(
                status.idle.as_deref(),
                Some("the lease ref could not be read")
            );
        }

        #[test]
        fn a_guard_that_fails_in_the_middle_of_a_pass_stops_every_further_change() {
            // Every cut point, from the first check to past the last: no change on GitHub or of
            // a worker is made once the guard has failed, and the pass says the lease was lost.
            let orphaned = |rig: &mut Rig| {
                // A finished run whose workspace a machine heard from again still holds.
                let run = rig.run_in(4, 1, State::Building);
                rig.ledger
                    .write(
                        &run,
                        &Event::Finished {
                            outcome: crate::factory::ledger::events::Outcome::PrOpen,
                            total_ms: 0,
                            gate_attempts: 0,
                            review_cycles: None,
                            pr_url: None,
                            pr_end: None,
                        },
                        &Cause::new(CauseStep::Start, "test"),
                    )
                    .unwrap();
                rig.view = FleetView::builder()
                    .online(me())
                    .online(builder_machine())
                    .sample(sample(builder_machine(), 0))
                    .held(
                        builder_machine(),
                        MachineHeld {
                            logins: vec![LOGIN.into()],
                            skill: true,
                        },
                    )
                    .workspace(Record {
                        owner: builder_machine(),
                        version: 1,
                        deleted: false,
                        body: Workspace {
                            id: WorkspaceId(uuid::Uuid::from_u128(0x44)),
                            project: project_id(),
                            name: "fac/4-1".into(),
                            branch: "fac/4-1".into(),
                            base: "main".into(),
                            setup: None,
                        },
                    })
                    .build();
            };
            let mut whole = Rig::new();
            for n in [5, 6, 7] {
                whole.hub.ready(n);
            }
            orphaned(&mut whole);
            whole.pass();
            assert!(
                whole
                    .worker
                    .inner
                    .calls()
                    .iter()
                    .any(|call| matches!(call, Call::Archive { .. })),
                "the whole pass archives the orphan"
            );
            let writes = whole.hub.writes().len();
            assert!(writes > 0);
            for checks in 0..=writes + 20 {
                let mut rig = Rig::new();
                for n in [5, 6, 7] {
                    rig.hub.ready(n);
                }
                orphaned(&mut rig);
                rig.guard_for(checks);
                let report = rig.pass();
                assert!(
                    rig.hub.late_writes().is_empty(),
                    "after {checks} checks: {:?}",
                    rig.hub.late_writes()
                );
                assert!(
                    rig.worker.late.lock().unwrap().is_empty(),
                    "after {checks} checks: {:?}",
                    rig.worker.late.lock().unwrap()
                );
                if rig.hub.lost.load(Ordering::SeqCst) {
                    // Lost at the pass's own check of the lease, or in the middle of it.
                    assert!(
                        report.lease_lost || report.generation.is_none(),
                        "after {checks} checks"
                    );
                    assert!(!rig.status().holds, "after {checks} checks");
                }
            }
        }
    }

    // ----- claiming new issues -----

    mod claiming_new_issues {
        use super::*;

        #[test]
        fn ready_issues_are_claimed_oldest_first() {
            let mut rig = Rig::new();
            for n in [9, 5, 7] {
                rig.hub.ready(n);
            }
            let report = rig.pass();
            assert_eq!(rig.claimed_issues(), vec![5, 7, 9]);
            assert_eq!(
                report.claimed,
                [5, 7, 9]
                    .iter()
                    .map(|n| RunId::new(REPO, *n, 1).unwrap())
                    .collect::<Vec<_>>()
            );
            for n in [5, 7, 9] {
                assert!(rig.hub.labels(n).contains(&"factory:triage".to_string()));
            }
        }

        #[test]
        fn no_more_issues_are_claimed_than_max_concurrent_runs_allows() {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.max_runs = 2);
            for n in [5, 6, 7] {
                rig.hub.ready(n);
            }
            let report = rig.pass();
            assert_eq!(rig.claimed_issues(), vec![5, 6]);
            assert_eq!(waiting_reason(&report, 7).as_deref(), Some(NO_SLOT));
        }

        #[test]
        fn runs_that_are_already_active_use_up_the_slots() {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.max_runs = 2);
            rig.hub.issue(3, &["factory:building"]);
            rig.run_in(3, 1, State::Building);
            for n in [5, 6] {
                rig.hub.ready(n);
            }
            rig.pass();
            assert_eq!(rig.claimed_issues(), vec![5]);
        }

        #[test]
        fn a_run_in_any_watched_repo_uses_a_slot() {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.max_runs = 1);
            let other = rig.book.ticket(other_project());
            rig.book.begin(
                other,
                Held {
                    runs: 1,
                    gates: 0,
                    placed: vec![],
                },
            );
            rig.hub.ready(5);
            let report = rig.pass();
            assert!(rig.claimed_issues().is_empty());
            assert_eq!(waiting_reason(&report, 5).as_deref(), Some(NO_SLOT));
        }

        #[test]
        fn an_issue_that_was_not_claimed_does_not_use_a_slot() {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.max_runs = 1);
            rig.hub.ready(5);
            rig.hub
                .edit(5, |issue| issue.assignees = vec!["someone".into()]);
            rig.hub.ready(6);
            let report = rig.pass();
            assert_eq!(rig.claimed_issues(), vec![6]);
            assert!(
                waiting_reason(&report, 5).is_some_and(|r| r.contains("already assigned")),
                "{report:?}"
            );
        }

        #[test]
        fn a_claimed_issue_gets_a_line_naming_its_run() {
            let mut rig = Rig::new();
            rig.hub.ready(5);
            let report = rig.pass();
            let run = RunId::new(REPO, 5, 1).unwrap();
            assert!(
                report
                    .runs
                    .iter()
                    .any(|(r, said)| *r == run && said.contains("claimed")),
                "{report:?}"
            );
        }

        #[test]
        fn a_claim_refused_for_no_room_says_it_waits_counts_no_error_and_claims_nothing_more_from_that_repo()
         {
            let mut rig = Rig::new();
            rig.view = FleetView::builder()
                .online(me())
                .online(builder_machine())
                .sample(sample(builder_machine(), 8))
                .held(
                    builder_machine(),
                    MachineHeld {
                        logins: vec![LOGIN.into()],
                        skill: true,
                    },
                )
                .build();
            for n in [5, 6] {
                rig.hub.ready(n);
            }
            let report = rig.pass();
            assert!(rig.claimed_issues().is_empty());
            assert!(
                rig.hub
                    .writes()
                    .iter()
                    .all(|w| !w.contains("foundry:claim"))
            );
            assert_eq!(report.failed, None);
            for n in [5, 6] {
                assert!(
                    waiting_reason(&report, n).is_some_and(|r| r.contains("no machine")),
                    "{report:?}"
                );
            }
        }

        #[test]
        fn a_repo_whose_ready_issues_cannot_be_listed_is_an_error_and_the_others_go_on() {
            let mut rig = Rig::new();
            rig.hub.ready(5);
            rig.hub.fail("factory:ready", 1);
            let report = rig.pass();
            assert!(report.failed.is_some());
            assert!(rig.claimed_issues().is_empty());
            // The next pass of the project goes on.
            let report = rig.pass();
            assert_eq!(report.failed, None);
            assert_eq!(rig.claimed_issues(), vec![5]);
        }

        #[test]
        fn a_repo_whose_config_cannot_be_loaded_is_reported_and_skipped_while_others_go_on() {
            let mut rig = Rig::new();
            rig.hub.lock().config = "factory: [not, a, map]\n".into();
            rig.hub.ready(5);
            let report = rig.pass();
            assert!(rig.claimed_issues().is_empty());
            assert!(rig.hub.writes().is_empty());
            assert_eq!(report.failed, None);
            let status = rig.status();
            assert!(status.holds);
            assert!(
                status
                    .idle
                    .as_deref()
                    .is_some_and(|i| i.contains(".dw/config.yml")),
                "{status:?}"
            );
        }
    }

    mod a_repo_whose_config_is_read {
        use super::*;

        #[test]
        fn the_config_is_read_again_only_when_the_default_branch_moved() {
            let mut rig = Rig::new();
            let reads = |rig: &Rig| {
                rig.hub
                    .calls()
                    .iter()
                    .filter(|args| args.iter().any(|a| a.contains("contents/.dw/config.yml")))
                    .count()
            };
            rig.pass();
            rig.pass();
            assert_eq!(reads(&rig), 1);
            rig.hub.lock().head = SHA2.into();
            rig.pass();
            assert_eq!(reads(&rig), 2);
        }

        #[test]
        fn three_stale_passes_in_a_row_raise_the_item_and_the_next_good_load_clears_it() {
            let mut rig = Rig::new();
            rig.pass();
            assert_eq!(rig.status().idle, None);
            rig.hub.fail("git/ref/heads/main", 3);
            rig.pass();
            rig.pass();
            assert_eq!(rig.status().idle, None, "two failed passes raise nothing");
            rig.pass();
            assert!(rig.status().holds);
            assert!(
                rig.status().idle.is_some(),
                "the third failed pass sets idle"
            );
            rig.pass();
            assert_eq!(rig.status().idle, None);
        }

        #[test]
        fn a_repo_that_sets_factory_owner_is_claimed_from_and_the_status_says_it_is_ignored() {
            let mut rig = Rig::new();
            rig.hub.lock().config = format!("{CONFIG}  owner: another-machine\n");
            rig.hub.ready(5);
            rig.pass();
            assert_eq!(rig.claimed_issues(), vec![5]);
            assert!(rig.status().owner_ignored);
        }
    }

    mod a_ready_issue_whose_older_run_is_gating {
        use super::*;

        #[test]
        fn is_not_claimed_again_says_gate_running_and_is_neither_blocked_nor_held_back() {
            let mut rig = Rig::new();
            let run = rig.run_in(5, 1, State::Building);
            rig.ledger
                .write(
                    &run,
                    &Event::StepChanged {
                        step: format!("gate-requested 1 1 {SHA} {SHA} {}", builder_machine()),
                    },
                    &Cause::new(CauseStep::Gate, "test"),
                )
                .unwrap();
            rig.hub.ready(5);
            let report = rig.pass();
            assert!(rig.claimed_issues().is_empty());
            assert_eq!(waiting_reason(&report, 5).as_deref(), Some(GATE_RUNNING));
            assert!(
                !rig.hub
                    .calls()
                    .iter()
                    .any(|args| args.iter().any(|a| a.contains("5/dependencies")))
            );
        }
    }

    mod claims_that_have_no_run_in_the_ledger {
        use super::*;

        #[test]
        fn a_triage_issue_of_ours_with_no_run_is_resumed_and_is_not_a_new_claim() {
            let mut rig = Rig::new();
            let run = RunId::new(REPO, 5, 1).unwrap();
            rig.hub.issue(5, &["factory:triage"]);
            rig.hub
                .edit(5, |issue| issue.assignees = vec![LOGIN.into()]);
            rig.hub.comment_as(
                5,
                LOGIN,
                &crate::factory::github::claim_comment(&run, &builder_machine()),
            );
            let report = rig.pass();
            assert!(rig.claimed_issues().is_empty());
            assert!(report.claimed.is_empty());
            assert!(report.foreign.is_empty());
            // The takeover recorded the claim and the build workspace was asked of its machine.
            assert!(!rig.ledger.read(&run).unwrap().is_empty());
            assert_eq!(
                rig.creates()
                    .iter()
                    .map(|c| c.name.clone())
                    .collect::<Vec<_>>(),
                vec!["fac/5-1".to_string()]
            );
        }

        #[test]
        fn a_run_named_by_a_workspace_and_no_event_is_carried_on_by_its_claim() {
            let mut rig = Rig::new();
            let run = RunId::new(REPO, 5, 1).unwrap();
            rig.hub.issue(5, &["factory:triage"]);
            rig.hub
                .edit(5, |issue| issue.assignees = vec![LOGIN.into()]);
            rig.hub.comment_as(
                5,
                LOGIN,
                &crate::factory::github::claim_comment(&run, &builder_machine()),
            );
            // The claim's build workspace, with its setup terminal, is on the machine the comment
            // names: a record with no setup would have the create asked again.
            rig.view = FleetView::builder()
                .online(me())
                .online(builder_machine())
                .workspace(Record {
                    owner: builder_machine(),
                    version: 1,
                    deleted: false,
                    body: Workspace {
                        id: WorkspaceId(uuid::Uuid::from_u128(0x77)),
                        project: project_id(),
                        name: "fac/5-1".into(),
                        branch: "fac/5-1".into(),
                        base: "main".into(),
                        setup: Some(TerminalId(uuid::Uuid::from_u128(0x78))),
                    },
                })
                .build();
            rig.pass();
            assert!(!rig.ledger.read(&run).unwrap().is_empty());
            assert!(
                rig.creates().iter().all(|c| c.name != "fac/5-1"),
                "no second workspace: {:?}",
                rig.creates()
            );
        }
    }

    mod two_factories_at_once {
        use super::*;

        #[test]
        fn a_foreign_claim_is_listed_and_its_issue_untouched() {
            let mut rig = Rig::new();
            rig.hub.issue(4, &["factory:building"]);
            rig.hub
                .edit(4, |issue| issue.assignees = vec![LOGIN.into()]);
            rig.hub.comment_as(
                4,
                LOGIN,
                "Claimed by machine vps.\n\n<!-- softfac:claim vps -->",
            );
            let report = rig.pass();
            assert_eq!(
                report.foreign,
                vec![ForeignClaim {
                    issue: 4,
                    title: "Issue 4".into(),
                    state: "building".into(),
                }]
            );
            assert_eq!(rig.status().foreign, report.foreign);
            assert!(
                rig.hub
                    .writes()
                    .iter()
                    .all(|w| !w.contains(" 4 ") && !w.contains("/4/")),
                "{:?}",
                rig.hub.writes()
            );
            assert!(rig.creates().is_empty());
        }

        /// The login's newest claim comment on issue 5 is a Foundry claim, of a run of issue 9
        /// or of another repo (copied, or edited by hand): it is a foreign claim. No run is
        /// carried on from it.
        #[test]
        fn a_foundry_claim_naming_another_issue_or_repo_is_foreign() {
            for named in [
                RunId::new(REPO, 9, 1).unwrap(),
                RunId::new("rawphp/elsewhere", 5, 1).unwrap(),
            ] {
                let mut rig = Rig::new();
                rig.hub.issue(5, &["factory:triage"]);
                rig.hub
                    .edit(5, |issue| issue.assignees = vec![LOGIN.into()]);
                rig.hub.comment_as(
                    5,
                    LOGIN,
                    &crate::factory::github::claim_comment(&named, &builder_machine()),
                );
                let report = rig.pass();
                assert_eq!(report.failed, None, "{named}: {report:?}");
                assert_eq!(
                    report.foreign.iter().map(|c| c.issue).collect::<Vec<_>>(),
                    vec![5],
                    "{named}"
                );
                assert!(rig.ledger.read(&named).unwrap().is_empty(), "{named}");
                assert!(
                    rig.ledger
                        .read(&RunId::new(REPO, 5, 1).unwrap())
                        .unwrap()
                        .is_empty(),
                    "{named}"
                );
                assert!(rig.creates().is_empty(), "{named}: {:?}", rig.creates());
                assert!(
                    rig.hub.writes().iter().all(|w| !w.contains(" 5 ")),
                    "{named}: {:?}",
                    rig.hub.writes()
                );
            }
        }

        #[test]
        fn a_claim_marker_in_another_logins_comment_is_no_claim_of_ours() {
            let mut rig = Rig::new();
            let run = RunId::new(REPO, 4, 1).unwrap();
            rig.hub.issue(4, &["factory:triage"]);
            rig.hub.comment_as(
                4,
                "mallory",
                &crate::factory::github::claim_comment(&run, &builder_machine()),
            );
            let report = rig.pass();
            // No claim of the login is on the issue, so it is neither ours nor foreign.
            assert!(report.foreign.is_empty());
            assert!(rig.ledger.read(&run).unwrap().is_empty());
            assert!(rig.creates().is_empty());
        }
    }

    mod issues_labelled_postponed {
        use super::*;

        #[test]
        fn a_ready_issue_labelled_postponed_is_not_claimed_and_its_blockers_are_not_read() {
            let mut rig = Rig::new();
            rig.hub.issue(5, &["factory:ready", "postponed"]);
            let report = rig.pass();
            assert!(rig.claimed_issues().is_empty());
            assert!(waiting_reason(&report, 5).is_some_and(|r| r.contains("postponed")));
            assert!(
                !rig.hub
                    .calls()
                    .iter()
                    .any(|args| args.iter().any(|a| a.contains("dependencies")))
            );
        }

        #[test]
        fn the_label_matches_in_any_case_and_only_as_the_whole_name() {
            let mut rig = Rig::new();
            rig.hub.issue(5, &["factory:ready", "Postponed"]);
            rig.hub.issue(6, &["factory:ready", "postponed-later"]);
            rig.pass();
            assert_eq!(rig.claimed_issues(), vec![6]);
        }

        #[test]
        fn a_postponed_issue_takes_no_slot_and_is_not_counted_against_the_limit() {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.max_runs = 1);
            rig.hub.issue(5, &["factory:ready", "postponed"]);
            rig.hub.ready(6);
            rig.pass();
            assert_eq!(rig.claimed_issues(), vec![6]);
        }
    }

    mod issues_blocked_by_an_open_issue {
        use super::*;

        #[test]
        fn an_issue_blocked_by_an_open_issue_is_not_claimed() {
            let mut rig = Rig::new();
            rig.hub.ready(5);
            rig.hub.issue(2, &[]);
            rig.hub.edit(5, |issue| issue.linked = vec![2]);
            let report = rig.pass();
            assert!(rig.claimed_issues().is_empty());
            assert!(
                waiting_reason(&report, 5).is_some_and(|r| r.contains("#2")),
                "{report:?}"
            );
        }

        #[test]
        fn a_blocked_issue_uses_no_slot_and_the_pass_goes_on_to_the_next() {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.max_runs = 1);
            rig.hub.ready(5);
            rig.hub.issue(2, &[]);
            rig.hub.edit(5, |issue| issue.linked = vec![2]);
            rig.hub.ready(6);
            rig.pass();
            assert_eq!(rig.claimed_issues(), vec![6]);
        }

        #[test]
        fn with_no_slot_free_no_blockers_are_read_and_every_issue_left_is_counted() {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.max_runs = 0);
            for n in [5, 6] {
                rig.hub.ready(n);
            }
            let report = rig.pass();
            assert!(
                !rig.hub
                    .calls()
                    .iter()
                    .any(|args| args.iter().any(|a| a.contains("dependencies")))
            );
            for n in [5, 6] {
                assert_eq!(waiting_reason(&report, n).as_deref(), Some(NO_SLOT));
            }
        }

        #[test]
        fn an_issue_whose_blockers_cannot_be_read_is_not_claimed_and_the_pass_goes_on() {
            // The plan's rule: a tracker error ends the project's pass, which goes on next pass.
            let mut rig = Rig::new();
            rig.hub.ready(5);
            rig.hub.fail("dependencies", 1);
            let report = rig.pass();
            assert!(report.failed.is_some());
            assert!(rig.claimed_issues().is_empty());
            rig.pass();
            assert_eq!(rig.claimed_issues(), vec![5]);
        }
    }

    mod issues_whose_depends_on_text_names_an_open_issue {
        use super::*;

        #[test]
        fn an_issue_whose_depends_on_names_an_open_issue_is_not_claimed() {
            let mut rig = Rig::new();
            rig.hub.ready(5);
            rig.hub.issue(2, &[]);
            rig.hub.edit(5, |issue| {
                issue.body = "Do it.\n\n## Depends on\n\n#2\n".into()
            });
            let report = rig.pass();
            assert!(rig.claimed_issues().is_empty());
            assert!(
                waiting_reason(&report, 5).is_some_and(|r| r.contains("Depends on")),
                "{report:?}"
            );
        }

        #[test]
        fn an_issue_whose_depends_on_names_nothing_open_is_claimed_as_normal() {
            let mut rig = Rig::new();
            rig.hub.ready(5);
            rig.hub.issue(2, &[]);
            rig.hub.edit(2, |issue| issue.closed = true);
            rig.hub.edit(5, |issue| {
                issue.body = "Do it.\n\n## Depends on\n\n#2\n".into()
            });
            rig.pass();
            assert_eq!(rig.claimed_issues(), vec![5]);
        }

        #[test]
        fn an_issue_blocked_by_a_link_does_not_have_its_depends_on_read() {
            let mut rig = Rig::new();
            rig.hub.ready(5);
            rig.hub.issue(2, &[]);
            rig.hub.edit(5, |issue| issue.linked = vec![2]);
            rig.pass();
            assert!(!rig.hub.calls().iter().any(|args| {
                args.len() == 2 && args[0] == "api" && args[1] == format!("repos/{REPO}/issues/2")
            }));
        }
    }

    mod the_limit_said_out_loud {
        use super::*;

        #[test]
        fn issues_left_unclaimed_because_the_limit_is_reached_wait_with_why() {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.max_runs = 1);
            for n in [5, 6, 7] {
                rig.hub.ready(n);
            }
            rig.pass();
            let ready = rig.status().ready;
            assert_eq!(
                ready
                    .iter()
                    .map(|w| (w.issue, w.reason.as_str()))
                    .collect::<Vec<_>>(),
                vec![(6, NO_SLOT), (7, NO_SLOT)]
            );
        }
    }

    mod the_hand_pause {
        use super::*;

        #[test]
        fn with_the_pause_no_ready_issue_of_any_repo_is_claimed_and_one_line_says_so() {
            for fleet in [true, false] {
                let mut rig = Rig::new();
                if fleet {
                    rig.fleet_settings(|f| f.paused = true);
                } else {
                    rig.project_settings(|p| p.paused = true);
                }
                for n in [5, 6] {
                    rig.hub.ready(n);
                }
                let report = rig.pass();
                assert!(rig.claimed_issues().is_empty());
                for n in [5, 6] {
                    assert!(
                        waiting_reason(&report, n).is_some_and(|r| r.contains("paused")),
                        "{report:?}"
                    );
                }
            }
        }

        #[test]
        fn a_paused_project_claims_nothing_and_its_runs_carry_on() {
            let mut rig = Rig::new();
            rig.hub.ready(5);
            rig.pass();
            assert_eq!(rig.claimed_issues(), vec![5]);
            let before = rig.creates().len();
            rig.project_settings(|p| p.paused = true);
            rig.hub.ready(6);
            let report = rig.pass();
            assert_eq!(rig.claimed_issues(), vec![5], "nothing more is claimed");
            let run = RunId::new(REPO, 5, 1).unwrap();
            assert!(
                report.runs.iter().any(|(r, _)| *r == run),
                "the run is worked: {report:?}"
            );
            assert!(
                report
                    .runs
                    .iter()
                    .any(|(r, said)| *r == run && said.starts_with("start:")),
                "its start step ran: {report:?}"
            );
            let _ = before;
        }
    }

    mod runs_that_are_confirmed_and_still_in_triage {
        use super::*;

        #[test]
        fn a_triage_run_is_worked_before_new_issues_are_claimed() {
            let mut rig = Rig::new();
            rig.hub.issue(3, &["factory:triage"]);
            rig.hub
                .edit(3, |issue| issue.assignees = vec![LOGIN.into()]);
            rig.run_in(3, 1, State::Triage);
            rig.hub.ready(5);
            rig.pass();
            let names: Vec<String> = rig.creates().iter().map(|c| c.name.clone()).collect();
            assert_eq!(names, vec!["fac/3-1".to_string(), "fac/5-1".to_string()]);
        }
    }

    mod following_a_building_run {
        use super::*;

        pub(super) fn building_with_report(rig: &mut Rig) -> RunId {
            let run = rig.run_in(5, 1, State::Building);
            let workspace = WorkspaceId(uuid::Uuid::from_u128(0x55));
            let session = SessionId(uuid::Uuid::from_u128(0x56));
            let cause = Cause::new(CauseStep::Start, "test");
            rig.ledger
                .write(
                    &run,
                    &Event::StepChanged {
                        step: format!("workspace-created {workspace}"),
                    },
                    &cause,
                )
                .unwrap();
            rig.ledger
                .write(
                    &run,
                    &Event::SessionLaunched {
                        thread_id: session.to_string(),
                        worktree: None,
                        branch: Some("fac/5-1".into()),
                        model_instance: None,
                        model: None,
                        model_options: None,
                        resumed_from: None,
                        base: Some("main".into()),
                        title: Some("Issue 5".into()),
                    },
                    &cause,
                )
                .unwrap();
            rig.view = FleetView::builder()
                .online(me())
                .online(builder_machine())
                .sample(sample(builder_machine(), 1))
                .held(
                    builder_machine(),
                    MachineHeld {
                        logins: vec![LOGIN.into()],
                        skill: true,
                    },
                )
                .workspace(Record {
                    owner: builder_machine(),
                    version: 1,
                    deleted: false,
                    body: Workspace {
                        id: workspace,
                        project: project_id(),
                        name: "fac/5-1".into(),
                        branch: "fac/5-1".into(),
                        base: "main".into(),
                        setup: None,
                    },
                })
                .session(Record {
                    owner: builder_machine(),
                    version: 2,
                    deleted: false,
                    body: Session {
                        id: session,
                        workspace,
                        terminal: TerminalId(uuid::Uuid::from_u128(0x57)),
                        model: "opus".into(),
                        state: SessionState::Idle,
                    },
                })
                .turn(SessionTurn {
                    session,
                    workspace,
                    project: project_id(),
                    turn: 1,
                    prompts: 1,
                    question: false,
                })
                .build();
            rig.hub.issue(5, &["factory:building"]);
            rig.hub
                .edit(5, |issue| issue.assignees = vec![LOGIN.into()]);
            run
        }

        #[test]
        fn a_session_that_reports_its_pull_request_has_its_gate_started_in_the_same_pass() {
            let mut rig = Rig::new();
            let run = building_with_report(&mut rig);
            rig.hub.lock().pulls.insert(12, "fac/5-1".into());
            rig.worker.inner.on_turn(Ok(TurnText {
                message: format!("Done.\nFACTORY-DONE: https://github.com/{REPO}/pull/12"),
                question: None,
            }));
            let report = rig.pass();
            let said: Vec<&String> = report
                .runs
                .iter()
                .filter(|(r, _)| *r == run)
                .map(|(_, s)| s)
                .collect();
            assert!(
                said.iter().any(|s| s.contains("gate")),
                "the gate step ran for the run: {report:?}"
            );
        }
    }

    mod the_fleets_gate_slots {
        use super::following_a_building_run::building_with_report;
        use super::*;

        /// What the pass said of `run`.
        fn said_of(report: &PassReport, run: &RunId) -> Vec<String> {
            report
                .runs
                .iter()
                .filter(|(r, _)| r == run)
                .map(|(_, s)| s.clone())
                .collect()
        }

        /// A run ready for its gate when another project's runs hold `others` of the fleet's
        /// one gate slot: what the pass said of it.
        fn ready_for_its_gate_while_another_project_holds(others: u32) -> Vec<String> {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.max_gates = 1);
            rig.book.seed(
                other_project(),
                Held {
                    runs: 0,
                    gates: others,
                    placed: vec![],
                },
            );
            let run = building_with_report(&mut rig);
            rig.hub.lock().pulls.insert(12, "fac/5-1".into());
            rig.worker.inner.on_turn(Ok(TurnText {
                message: format!("Done.\nFACTORY-DONE: https://github.com/{REPO}/pull/12"),
                question: None,
            }));
            let report = rig.pass();
            assert_eq!(report.failed, None, "{report:?}");
            said_of(&report, &run)
        }

        #[test]
        fn a_run_ready_for_its_gate_waits_while_another_project_holds_every_gate_slot() {
            let held = ready_for_its_gate_while_another_project_holds(1);
            assert!(
                held.iter().any(|s| s.contains("gate: Queued")),
                "the fleet's one gate slot is another project's: {held:?}"
            );
            let free = ready_for_its_gate_while_another_project_holds(0);
            assert!(
                free.iter()
                    .any(|s| s.contains("gate: ") && !s.contains("gate: Queued")),
                "with the slot free the gate is placed: {free:?}"
            );
        }

        #[test]
        fn two_projects_never_both_take_the_last_gate_slot() {
            let book = Book::default();
            let a = book.ticket(project_id());
            let b = book.ticket(other_project());
            book.begin(
                a,
                Held {
                    runs: 0,
                    gates: 0,
                    placed: vec![],
                },
            )
            .unwrap();
            book.begin(
                b,
                Held {
                    runs: 0,
                    gates: 1,
                    placed: vec![],
                },
            )
            .unwrap();
            assert!(book.take_gate(a, 2));
            assert!(!book.take_gate(b, 2), "the fleet holds 2 of 2");
            assert!(!book.take_gate(a, 2), "nor the project that took the last");
            book.give_gate(a);
            assert!(book.take_gate(b, 2), "a gate given back is free again");
            book.forget(project_id());
            assert!(
                !book.take_gate(a, 5),
                "a pass of a forgotten project takes none"
            );
        }
    }

    mod orphans {
        use super::*;

        #[test]
        fn a_machine_heard_from_again_has_the_orphans_of_a_run_archived() {
            let mut rig = Rig::new();
            let run = rig.run_in(5, 1, State::Building);
            let cause = Cause::new(CauseStep::Start, "test");
            rig.ledger
                .write(
                    &run,
                    &Event::Finished {
                        outcome: crate::factory::ledger::events::Outcome::PrOpen,
                        total_ms: 0,
                        gate_attempts: 0,
                        review_cycles: None,
                        pr_url: None,
                        pr_end: None,
                    },
                    &cause,
                )
                .unwrap_or_else(|e| panic!("{e}"));
            let orphan = WorkspaceId(uuid::Uuid::from_u128(0x99));
            let with = |online: bool| {
                let mut view = FleetView::builder().online(me());
                if online {
                    view = view.online(builder_machine());
                }
                view.workspace(Record {
                    owner: builder_machine(),
                    version: 1,
                    deleted: false,
                    body: Workspace {
                        id: orphan,
                        project: project_id(),
                        name: "fac/5-1".into(),
                        branch: "fac/5-1".into(),
                        base: "main".into(),
                        setup: None,
                    },
                })
                .build()
            };
            rig.view = with(false);
            rig.pass();
            rig.view = with(true);
            let report = rig.pass();
            assert_eq!(report.archived, vec![orphan]);
            assert!(
                rig.worker
                    .inner
                    .calls()
                    .contains(&Call::Archive { workspace: orphan })
            );
        }
    }

    mod the_status {
        use super::*;

        #[test]
        fn a_holding_status_is_written_each_pass_with_a_pass_count_that_only_rises() {
            let mut rig = Rig::new();
            rig.pass();
            let first = rig.status();
            assert!(first.holds);
            assert_eq!(first.generation, 1);
            rig.pass();
            assert!(rig.status().pass > first.pass);
            // A restart starts from the count the store holds.
            rig.memo = Memo::default();
            rig.pass();
            assert!(rig.status().pass > first.pass + 1);
        }

        #[test]
        fn the_status_names_the_live_runs_and_the_ready_queue() {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.max_runs = 1);
            rig.hub.ready(5);
            rig.hub.ready(6);
            rig.pass();
            let status = rig.status();
            assert_eq!(
                status
                    .runs
                    .iter()
                    .map(|r| r.run.clone())
                    .collect::<Vec<_>>(),
                vec![RunId::new(REPO, 5, 1).unwrap()]
            );
            assert_eq!(status.ready.len(), 1);
            assert_eq!(status.ready[0].issue, 6);
        }
    }
    // ----- runner cases ported in the second cycle -----

    mod claiming_new_issues_more {
        use super::*;

        #[test]
        fn a_building_run_whose_pull_request_waits_for_the_gate_uses_no_slot() {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.max_runs = 1);
            rig.hub.issue(3, &["factory:building"]);
            let run = rig.run_in(3, 1, State::Building);
            rig.ledger
                .write(
                    &run,
                    &Event::PrOpened {
                        pr: 12,
                        url: format!("https://github.com/{REPO}/pull/12"),
                    },
                    &Cause::new(CauseStep::Advance, "test"),
                )
                .unwrap();
            rig.hub.ready(5);
            rig.pass();
            assert_eq!(rig.claimed_issues(), vec![5]);
        }

        #[test]
        fn a_failed_start_is_an_error_line_and_the_next_issue_is_tried() {
            let mut rig = Rig::new();
            rig.hub.ready(5);
            rig.hub.ready(6);
            rig.worker
                .inner
                .on_create(Err(WorkerError::Failed("disk full".into())));
            let report = rig.pass();
            assert_eq!(rig.claimed_issues(), vec![5, 6]);
            let first = RunId::new(REPO, 5, 1).unwrap();
            assert!(
                report.runs.iter().any(|(r, said)| *r == first
                    && said.contains("claimed")
                    && said.contains("disk full")),
                "the worker's error is on the run's line: {report:?}"
            );
        }
    }

    mod claims_that_have_no_run_in_the_ledger_more {
        use super::*;

        fn claimed_on_github(rig: &Rig, issue: u64, attempt: u32) -> RunId {
            let run = RunId::new(REPO, issue, attempt).unwrap();
            rig.hub.issue(issue, &["factory:triage"]);
            rig.hub.edit(issue, |i| i.assignees = vec![LOGIN.into()]);
            rig.hub.comment_as(
                issue,
                LOGIN,
                &crate::factory::github::claim_comment(&run, &builder_machine()),
            );
            run
        }

        fn names(rig: &Rig) -> Vec<String> {
            rig.creates().iter().map(|c| c.name.clone()).collect()
        }

        #[test]
        fn a_triage_issue_that_has_an_unfinished_run_is_resumed_by_the_run_step_alone() {
            let mut rig = Rig::new();
            rig.hub.issue(5, &["factory:triage"]);
            rig.hub.edit(5, |i| i.assignees = vec![LOGIN.into()]);
            let run = rig.run_in(5, 1, State::Triage);
            let report = rig.pass();
            let said: Vec<&String> = report
                .runs
                .iter()
                .filter(|(r, _)| *r == run)
                .map(|(_, s)| s)
                .collect();
            assert_eq!(said.len(), 1, "{report:?}");
            assert!(said[0].starts_with("start:"), "{report:?}");
            assert_eq!(names(&rig), vec!["fac/5-1".to_string()]);
        }

        #[test]
        fn a_triage_issue_whose_only_run_is_not_confirmed_is_resumed_again() {
            let mut rig = Rig::new();
            let run = claimed_on_github(&rig, 5, 1);
            rig.ledger
                .write(
                    &run,
                    &Event::Claimed {
                        machine: builder_machine().to_string(),
                        github_user: LOGIN.into(),
                    },
                    &Cause::new(CauseStep::Start, "test"),
                )
                .unwrap();
            let report = rig.pass();
            assert!(
                report
                    .runs
                    .iter()
                    .any(|(r, said)| *r == run && said.starts_with("carried on")),
                "{report:?}"
            );
        }

        #[test]
        fn a_run_that_is_not_confirmed_takes_no_slot() {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.max_runs = 1);
            let run = RunId::new(REPO, 3, 1).unwrap();
            rig.ledger
                .write(
                    &run,
                    &Event::Claimed {
                        machine: builder_machine().to_string(),
                        github_user: LOGIN.into(),
                    },
                    &Cause::new(CauseStep::Start, "test"),
                )
                .unwrap();
            rig.hub.ready(5);
            rig.pass();
            assert_eq!(rig.claimed_issues(), vec![5]);
        }

        #[test]
        fn a_triage_issue_with_no_run_is_resumed_even_when_every_slot_is_taken() {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.max_runs = 0);
            claimed_on_github(&rig, 5, 1);
            rig.pass();
            assert_eq!(names(&rig), vec!["fac/5-1".to_string()]);
        }

        #[test]
        fn a_triage_issue_is_resumed_before_new_issues_are_claimed() {
            let mut rig = Rig::new();
            claimed_on_github(&rig, 5, 1);
            rig.hub.ready(3);
            rig.pass();
            assert_eq!(
                names(&rig),
                vec!["fac/5-1".to_string(), "fac/3-1".to_string()]
            );
        }

        #[test]
        fn a_triage_issue_that_start_leaves_alone_or_fails_on_is_said_so() {
            let mut rig = Rig::new();
            rig.hub.issue(5, &["factory:triage"]);
            let run = RunId::new(REPO, 5, 1).unwrap();
            // A workspace names the run and no claim comment does: start leaves it alone.
            rig.view = FleetView::builder()
                .online(me())
                .workspace(Record {
                    owner: builder_machine(),
                    version: 1,
                    deleted: false,
                    body: Workspace {
                        id: WorkspaceId(uuid::Uuid::from_u128(0x77)),
                        project: project_id(),
                        name: "fac/5-1".into(),
                        branch: "fac/5-1".into(),
                        base: "main".into(),
                        setup: None,
                    },
                })
                .build();
            let report = rig.pass();
            assert!(
                report
                    .runs
                    .iter()
                    .any(|(r, said)| *r == run && said.contains("Skipped")),
                "{report:?}"
            );
            assert!(report.foreign.is_empty());
        }

        #[test]
        fn triage_issues_that_cannot_be_listed_are_an_error_and_the_others_go_on() {
            // The plan's rule: the error ends the project's pass, and the next pass goes on.
            let mut rig = Rig::new();
            claimed_on_github(&rig, 5, 1);
            rig.hub.fail("factory:triage", 1);
            let report = rig.pass();
            assert!(report.failed.is_some());
            assert!(rig.creates().is_empty());
            rig.pass();
            assert_eq!(names(&rig), vec!["fac/5-1".to_string()]);
        }

        #[test]
        fn a_repo_whose_config_is_broken_has_its_triage_issues_left_alone() {
            let mut rig = Rig::new();
            rig.hub.lock().config = "factory: [not, a, map]\n".into();
            claimed_on_github(&rig, 5, 1);
            rig.pass();
            assert!(rig.creates().is_empty());
            assert!(
                !rig.hub
                    .calls()
                    .iter()
                    .any(|args| args.iter().any(|a| a.contains("/comments")))
            );
        }
    }

    mod issues_labelled_postponed_more {
        use super::*;

        #[test]
        fn the_same_issue_without_the_label_is_claimed() {
            let mut rig = Rig::new();
            rig.hub.ready(5);
            rig.pass();
            assert_eq!(rig.claimed_issues(), vec![5]);
        }

        #[test]
        fn a_building_run_on_an_issue_that_is_now_postponed_goes_on_as_before() {
            let mut rig = Rig::new();
            rig.hub.issue(5, &["factory:building", "postponed"]);
            rig.hub.edit(5, |i| i.assignees = vec![LOGIN.into()]);
            let run = rig.run_in(5, 1, State::Building);
            let report = rig.pass();
            // The run is worked as any building run: the label holds back claims only.
            let said: Vec<&String> = report
                .runs
                .iter()
                .filter(|(r, _)| *r == run)
                .map(|(_, s)| s)
                .collect();
            assert_eq!(said.len(), 1, "{report:?}");
            assert!(!said[0].contains("postponed"), "{report:?}");
        }

        #[test]
        fn while_claims_are_paused_a_postponed_issue_gets_no_line_and_no_event_of_its_own() {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.paused = true);
            rig.hub.issue(5, &["factory:ready", "postponed"]);
            let report = rig.pass();
            assert_eq!(
                waiting_reason(&report, 5).as_deref(),
                Some("claims are paused for the fleet")
            );
        }
    }

    mod issues_blocked_by_an_open_issue_more {
        use super::*;

        fn dependency_reads(rig: &Rig, issue: u64) -> usize {
            rig.hub
                .calls()
                .iter()
                .filter(|args| {
                    args.iter()
                        .any(|a| a.contains(&format!("issues/{issue}/dependencies")))
                })
                .count()
        }

        #[test]
        fn several_blockers_are_joined_and_one_in_another_repo_names_its_repo() {
            let mut rig = Rig::new();
            rig.hub.ready(5);
            rig.hub.issue(2, &[]);
            rig.hub.issue(3, &[]);
            rig.hub.edit(5, |i| {
                i.linked = vec![2, 3];
                i.linked_in = vec![("other/repo".into(), 9)];
            });
            let report = rig.pass();
            assert_eq!(
                waiting_reason(&report, 5).as_deref(),
                Some("blocked by #2, #3, other/repo#9")
            );
        }

        #[test]
        fn blockers_are_read_only_until_the_last_slot_is_taken() {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.max_runs = 1);
            rig.hub.ready(5);
            rig.hub.ready(6);
            rig.pass();
            assert_eq!(dependency_reads(&rig, 5), 1);
            assert_eq!(dependency_reads(&rig, 6), 0);
        }

        #[test]
        fn blockers_are_read_only_when_an_issue_is_about_to_be_claimed() {
            let mut rig = Rig::new();
            rig.project_settings(|p| p.paused = true);
            rig.hub.ready(5);
            rig.pass();
            assert_eq!(dependency_reads(&rig, 5), 0);
        }
    }

    mod issues_whose_depends_on_text_names_an_open_issue_more {
        use super::*;

        #[test]
        fn each_blocker_from_the_text_carries_the_suffix_and_one_in_another_repo_names_its_repo() {
            let mut rig = Rig::new();
            rig.hub.ready(5);
            rig.hub.issue(2, &[]);
            rig.hub.edit(5, |i| {
                i.body = "Do it.\n\n## Depends on\n\n- #2\n- other/repo#3\n".into()
            });
            let report = rig.pass();
            assert_eq!(
                waiting_reason(&report, 5).as_deref(),
                Some(
                    "blocked by #2 (from Depends on, add the GitHub link), other/repo#3 (from \
                     Depends on, add the GitHub link)"
                )
            );
        }

        #[test]
        fn an_issue_whose_depends_on_cannot_be_read_is_not_claimed_and_the_pass_goes_on() {
            // The plan's rule: a tracker error ends the project's pass, which goes on next pass.
            let mut rig = Rig::new();
            rig.hub.ready(5);
            rig.hub.fail("--json body", 1);
            let report = rig.pass();
            assert!(report.failed.is_some());
            assert!(rig.claimed_issues().is_empty());
            rig.pass();
            assert_eq!(rig.claimed_issues(), vec![5]);
        }
    }

    mod the_hand_pause_more {
        use super::*;

        #[test]
        fn with_the_pause_an_untracked_claim_is_still_handled() {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.paused = true);
            let run = RunId::new(REPO, 5, 1).unwrap();
            rig.hub.issue(5, &["factory:triage"]);
            rig.hub.edit(5, |i| i.assignees = vec![LOGIN.into()]);
            rig.hub.comment_as(
                5,
                LOGIN,
                &crate::factory::github::claim_comment(&run, &builder_machine()),
            );
            rig.pass();
            assert_eq!(
                rig.creates()
                    .iter()
                    .map(|c| c.name.clone())
                    .collect::<Vec<_>>(),
                vec!["fac/5-1".to_string()]
            );
        }

        #[test]
        fn without_the_pause_the_same_ready_issue_is_claimed_and_nothing_is_said_of_a_pause() {
            let mut rig = Rig::new();
            rig.hub.ready(5);
            let report = rig.pass();
            assert_eq!(rig.claimed_issues(), vec![5]);
            assert!(report.waiting.iter().all(|w| !w.reason.contains("paused")));
        }
    }

    mod a_run_whose_end_is_wound_up {
        use super::*;

        #[test]
        fn a_run_whose_reconcile_waits_on_a_machine_gets_no_other_step() {
            let mut rig = Rig::new();
            let run = super::following_a_building_run::building_with_report(&mut rig);
            // A person closed the issue; the archive of the run's workspace meets the link down.
            rig.hub.edit(5, |issue| issue.closed = true);
            for _ in 0..4 {
                rig.worker
                    .inner
                    .on_archive(Err(WorkerError::Unreachable("the link went down".into())));
            }
            let report = rig.pass();
            let said: Vec<&String> = report
                .runs
                .iter()
                .filter(|(r, _)| *r == run)
                .map(|(_, s)| s)
                .collect();
            assert!(said[0].starts_with("reconcile: Waiting"), "{report:?}");
            assert!(
                said.iter()
                    .all(|s| !s.contains("build:") && !s.contains("gate:")),
                "no other step of the run: {report:?}"
            );
        }
    }

    mod stuck_runs_failed_starts_review_rounds_and_the_fleets_slots {
        use super::*;

        /// A step line written to `run`'s ledger.
        fn step(rig: &Rig, run: &RunId, text: &str) {
            rig.ledger
                .write(
                    run,
                    &Event::StepChanged { step: text.into() },
                    &Cause::new(CauseStep::Start, "test"),
                )
                .unwrap();
        }

        /// The line the pass reported for `run`.
        fn said(report: &PassReport, run: &RunId) -> Vec<String> {
            report
                .runs
                .iter()
                .filter(|(r, _)| r == run)
                .map(|(_, said)| said.clone())
                .collect()
        }

        #[test]
        fn an_unfinished_stuck_run_is_reconciled_on_every_pass_before_the_stuck_sweep_is_due() {
            let mut rig = Rig::new();
            // A person moved the issue out of `factory:stuck`, and the path that took the run
            // there was cut short: its decision is in the ledger and its own code finishes it.
            rig.hub.issue(5, &["factory:building"]);
            rig.hub.edit(5, |i| i.assignees = vec![LOGIN.into()]);
            let run = rig.run_in(5, 1, State::Stuck);
            step(
                &rig,
                &run,
                "stuck-stop {\"code\":\"label-removed\",\"stop\":{\"reason\":\"a person moved it\"}}",
            );
            // The fake clock is at zero: STUCK_READ_EVERY has not passed since anything.
            let report = rig.pass();
            let lines = said(&report, &run);
            assert!(
                lines.iter().any(|l| l.starts_with("reconcile: Stopped")),
                "{report:?}"
            );
        }

        #[test]
        fn a_triage_issue_that_start_fails_on_is_noted_against_its_run_and_the_pass_goes_on() {
            let mut rig = Rig::new();
            let run = RunId::new(REPO, 5, 1).unwrap();
            rig.hub.issue(5, &["factory:triage"]);
            rig.hub.edit(5, |i| i.assignees = vec![LOGIN.into()]);
            rig.hub.comment_as(
                5,
                LOGIN,
                &crate::factory::github::claim_comment(&run, &builder_machine()),
            );
            rig.hub.ready(6);
            rig.worker
                .inner
                .on_create(Err(WorkerError::Failed("the worker broke".into())));
            let report = rig.pass();
            assert!(
                report
                    .runs
                    .iter()
                    .any(|(r, said)| *r == run && said.contains("the worker broke")),
                "{report:?}"
            );
            // The error is the run's: the pass is not failed, goes on to the claims and holds.
            assert!(report.failed.is_none(), "{report:?}");
            assert!(rig.status().holds);
            assert_eq!(rig.claimed_issues(), vec![6]);
        }

        /// GitHub as a feedback round reads it: the hub, with one review on the pull request.
        struct Reviewed(Arc<Hub>);

        impl crate::factory::github::Tracker for Reviewed {
            fn gh(&self, args: &[String]) -> Result<String, crate::factory::github::TrackerError> {
                // The hub does not know a pull request's draft toggle: it is taken as made.
                if args.first().map(String::as_str) == Some("pr")
                    && args.get(1).map(String::as_str) == Some("ready")
                {
                    return Ok(String::new());
                }
                self.0.gh(args)
            }

            fn pr_feedback(
                &self,
                _repo: &str,
                _pr: u64,
                _since: &str,
                _floor: &str,
                _instructors: &[String],
            ) -> Result<
                Vec<crate::factory::github::read::Feedback>,
                crate::factory::github::TrackerError,
            > {
                use crate::factory::github::read::{Feedback, FeedbackKind};
                Ok(vec![Feedback {
                    id: "review:11".into(),
                    author: "tom".into(),
                    kind: FeedbackKind::Review {
                        state: "CHANGES_REQUESTED".into(),
                    },
                    created_at: "2026-10-04T11:00:00Z".into(),
                    body: "Please rename it.".into(),
                    inline: vec![],
                    inline_unread: 0,
                }])
            }

            fn activity_since(
                &self,
                _repo: &str,
                _issue: u64,
                _since: &str,
                _instructors: &[String],
            ) -> Result<crate::factory::github::read::Activity, crate::factory::github::TrackerError>
            {
                Ok(crate::factory::github::read::Activity {
                    comments: vec![],
                    body_edit: None,
                    dropped: vec![],
                })
            }
        }

        /// Issue `issue` has its run in `pr-open` with pull request 12, as the gate leaves it,
        /// and an instructor's review is on the pull request.
        fn reviewed_pr_open(rig: &mut Rig, issue: u64) -> RunId {
            let run = RunId::new(REPO, issue, 1).unwrap();
            let write = |event: Event| {
                rig.ledger
                    .write(&run, &event, &Cause::new(CauseStep::Start, "test"))
                    .unwrap();
            };
            let note = |text: &str| write(Event::StepChanged { step: text.into() });
            let moved = |from, to| write(Event::StateChanged { from, to });
            note(&format!("approved {LABELED_AT}"));
            write(Event::Claimed {
                machine: builder_machine().to_string(),
                github_user: LOGIN.into(),
            });
            moved(State::Ready, State::Triage);
            note("launch-model opus");
            note(&format!("workspace-requested fac/{issue}-1"));
            let old = uuid::Uuid::from_u128(0xa0);
            note(&format!("workspace-created {old}"));
            note("approval-rechecked");
            write(Event::SessionLaunched {
                thread_id: uuid::Uuid::from_u128(0x5e5).to_string(),
                worktree: Some(old.to_string()),
                branch: Some(format!("fac/{issue}-1")),
                model_instance: None,
                model: Some("opus".into()),
                model_options: None,
                resumed_from: None,
                base: Some("main".into()),
                title: Some("Make it faster".into()),
            });
            moved(State::Triage, State::Building);
            write(Event::PrOpened {
                pr: 12,
                url: "https://github.com/rawphp/softfac/pull/12".into(),
            });
            write(Event::GateCommand {
                attempt: 1,
                command: "cargo test".into(),
                exit_code: 0,
                duration_ms: 1000,
            });
            note(&format!("gate-passed {SHA} 1"));
            for text in [
                "build-session-ended",
                "build-archiving",
                "build-archived",
                "gate-results-posted",
            ] {
                note(text);
            }
            moved(State::Building, State::PrOpen);
            rig.hub.issue(issue, &["factory:pr-open"]);
            rig.hub.edit(issue, |i| i.assignees = vec![LOGIN.into()]);
            rig.hub.lock().pulls.insert(12, format!("fac/{issue}-1"));
            run
        }

        impl crate::factory::github::Commenting for Reviewed {
            fn hide(&self, text: &str) -> String {
                text.to_owned()
            }
        }

        /// [`Rig::pass`] with the review of [`Reviewed`] on the pull request.
        fn pass_reviewed(rig: &mut Rig) -> PassReport {
            let tracker = Reviewed(rig.hub.clone());
            let world = World {
                project: &rig.project,
                lease: &rig.lease,
                guard: rig.guard.as_ref(),
                tracker: &tracker,
                worker: &rig.worker,
                view: &rig.view,
                ledger: &rig.ledger,
                store: &rig.store,
                me: me(),
                shared: &rig.shared,
                book: &rig.book,
                clock: &rig.clock,
            };
            run(&world, &mut rig.memo)
        }

        #[test]
        fn a_round_that_is_due_takes_the_last_slot_before_a_ready_issue_is_claimed() {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.max_runs = 1);
            let run = reviewed_pr_open(&mut rig, 8);
            rig.hub.ready(5);
            let report = pass_reviewed(&mut rig);
            assert!(
                said(&report, &run)
                    .iter()
                    .any(|l| l.starts_with("feedback: Decided")),
                "the round starts: {report:?}"
            );
            assert!(rig.claimed_issues().is_empty(), "{report:?}");
            assert_eq!(waiting_reason(&report, 5).as_deref(), Some(NO_SLOT));
        }

        #[test]
        fn a_round_under_way_takes_no_run_slot_while_it_is_carried_on() {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.max_runs = 2);
            let run = reviewed_pr_open(&mut rig, 8);
            let first = pass_reviewed(&mut rig);
            assert!(
                said(&first, &run)
                    .iter()
                    .any(|l| l.starts_with("feedback: Decided")),
                "the round is decided: {first:?}"
            );
            // The round holds one of the two slots and is carried on; the other is free for
            // the claim, which a second slot taken for the round would have refused.
            rig.hub.ready(5);
            let second = pass_reviewed(&mut rig);
            assert!(
                said(&second, &run)
                    .iter()
                    .any(|l| l.starts_with("feedback: Preparing")),
                "{second:?}"
            );
            assert_eq!(rig.claimed_issues(), vec![5], "{second:?}");
        }
    }

    mod the_book {
        use super::*;

        fn held(runs: u32) -> Held {
            Held {
                runs,
                gates: 0,
                placed: vec![],
            }
        }

        #[test]
        fn two_projects_never_both_take_the_last_run_slot() {
            let book = Book::default();
            let a = book.ticket(project_id());
            let b = book.ticket(other_project());
            book.begin(a, held(1)).unwrap();
            book.begin(b, held(1)).unwrap();
            assert!(book.take_run(a, 3));
            assert!(!book.take_run(b, 3), "the fleet holds 3 of 3");
            book.give_run(a);
            assert!(book.take_run(b, 3));
        }

        #[test]
        fn a_pass_of_a_project_forgotten_since_it_began_changes_nothing() {
            let book = Book::default();
            let ticket = book.ticket(project_id());
            book.begin(ticket, held(1)).unwrap();
            book.forget(project_id());
            book.finish(ticket, held(3));
            assert!(!book.take_run(ticket, 3));
            book.place(ticket, builder_machine());
            let others = book.begin(book.ticket(other_project()), held(0)).unwrap();
            assert!(others.is_empty());
            assert_eq!(book.runs_free(3), 3);
        }

        #[test]
        fn a_project_forgotten_between_its_pass_s_ticket_and_its_begin_is_not_written_back() {
            let book = Book::default();
            let ticket = book.ticket(project_id());
            book.forget(project_id());
            assert!(book.begin(ticket, held(3)).is_none());
            assert_eq!(book.runs_free(3), 3);
        }

        #[test]
        fn only_the_passes_that_hold_the_lease_count_toward_idle() {
            let mut rig = Rig::new();
            rig.lease = LeaseState::HeldBy {
                machine: builder_machine(),
                generation: 4,
            };
            rig.guard = Box::new(Lost);
            rig.hub.fail("git/ref/heads/main", 4);
            for _ in 0..3 {
                rig.pass();
            }
            rig.lease = LeaseState::Held { generation: 5 };
            rig.guard = Box::new(AlwaysHeld(5));
            rig.pass();
            let status = rig.status();
            assert!(status.holds);
            assert_eq!(status.idle, None, "one failed pass that held the lease");
        }

        #[test]
        fn an_orphan_whose_archive_failed_is_tried_again_next_pass() {
            let mut rig = Rig::new();
            let run = rig.run_in(4, 1, State::Building);
            rig.ledger
                .write(
                    &run,
                    &Event::Finished {
                        outcome: crate::factory::ledger::events::Outcome::PrOpen,
                        total_ms: 0,
                        gate_attempts: 0,
                        review_cycles: None,
                        pr_url: None,
                        pr_end: None,
                    },
                    &Cause::new(CauseStep::Start, "test"),
                )
                .unwrap();
            let orphan = WorkspaceId(uuid::Uuid::from_u128(0x44));
            rig.view = FleetView::builder()
                .online(me())
                .online(builder_machine())
                .workspace(Record {
                    owner: builder_machine(),
                    version: 1,
                    deleted: false,
                    body: Workspace {
                        id: orphan,
                        project: project_id(),
                        name: "fac/4-1".into(),
                        branch: "fac/4-1".into(),
                        base: "main".into(),
                        setup: None,
                    },
                })
                .build();
            // The finished run's sweep and the orphan's archive both meet the link down.
            for _ in 0..2 {
                rig.worker
                    .inner
                    .on_archive(Err(WorkerError::Unreachable("the link went down".into())));
            }
            let first = rig.pass();
            assert!(first.archived.is_empty());
            let second = rig.pass();
            assert_eq!(second.archived, vec![orphan]);
        }

        #[test]
        fn a_config_that_does_not_load_has_the_known_secrets_hidden_in_its_idle_reason() {
            let mut rig = Rig::new();
            rig.tracker = Hiding::new(
                rig.hub.clone(),
                Arc::new(|text: &str| text.replace("s3cr3t", "***")),
            );
            rig.hub.lock().config = format!("{CONFIG}  autonomy: s3cr3t\n");
            rig.pass();
            let idle = rig.status().idle.unwrap_or_default();
            assert!(!idle.is_empty());
            assert!(!idle.contains("s3cr3t"), "{idle}");
        }
    }
    mod the_book_and_the_lease {
        use super::*;

        /// Holds the lease, and on its first check has the service stop the project: the
        /// project is forgotten in the book, as `serve` does after it releases the lease.
        struct ForgetsOnCheck {
            book: Arc<Book>,
            forgot: AtomicBool,
        }

        impl Guard for ForgetsOnCheck {
            fn check(&self) -> Result<u64, LeaseLost> {
                if !self.forgot.swap(true, Ordering::SeqCst) {
                    self.book.forget(project_id());
                }
                Ok(1)
            }
        }

        #[test]
        fn a_project_stopped_after_its_pass_checked_the_lease_is_not_written_back_to_the_book() {
            let mut rig = Rig::new();
            rig.run_in(3, 1, State::Building);
            rig.hub.issue(3, &["factory:building"]);
            let book = Arc::new(Book::default());
            let guard = ForgetsOnCheck {
                book: book.clone(),
                forgot: AtomicBool::new(false),
            };
            let world = World {
                project: &rig.project,
                lease: &rig.lease,
                guard: &guard,
                tracker: &rig.tracker,
                worker: &rig.worker,
                view: &rig.view,
                ledger: &rig.ledger,
                store: &rig.store,
                me: me(),
                shared: &rig.shared,
                book: &book,
                clock: &rig.clock,
            };
            let report = run(&world, &mut rig.memo);
            assert!(report.lease_lost, "{report:?}");
            assert_eq!(book.runs_free(3), 3, "the stopped project holds nothing");
        }

        #[test]
        fn a_pass_that_does_not_hold_the_lease_starts_the_count_of_failed_passes_again() {
            let mut rig = Rig::new();
            rig.hub.fail("git/ref/heads/main", 2);
            rig.pass();
            rig.pass();
            // A pass that reads GitHub well and does not hold the lease.
            rig.lease = LeaseState::HeldBy {
                machine: builder_machine(),
                generation: 4,
            };
            rig.guard = Box::new(Lost);
            rig.pass();
            rig.lease = LeaseState::Held { generation: 5 };
            rig.guard = Box::new(AlwaysHeld(5));
            rig.hub.fail("git/ref/heads/main", 1);
            rig.pass();
            assert_eq!(
                rig.status().idle,
                None,
                "one failed pass since the not-holding one"
            );
        }
    }

    mod a_run_this_build_cannot_read {
        use super::*;

        /// A run event row of `run` that this build cannot read, as a newer build leaves one.
        pub(super) fn unreadable_row(rig: &Rig, run: &RunId) {
            let owner = MachineId::random();
            let conn = rusqlite::Connection::open(rig._dir.path().join("foundry.db")).unwrap();
            conn.execute(
                "INSERT INTO factory_records (kind, id, owner, version, record)
                 VALUES ('run_event', ?1, ?2, 1, ?3)",
                rusqlite::params![
                    format!("{run}/{owner}/1"),
                    owner.to_string(),
                    r#"{"kind":"run_event","later":true}"#
                ],
            )
            .unwrap();
        }

        #[test]
        fn the_sweep_notes_it_and_the_pass_goes_on() {
            let mut rig = Rig::new();
            unreadable_row(&rig, &RunId::new(REPO, 5, 1).unwrap());
            let report = rig.pass();
            assert_eq!(report.failed, None, "{report:?}");
            assert!(
                report
                    .runs
                    .contains(&(RunId::new(REPO, 5, 1).unwrap(), UNREADABLE_SWEPT.into())),
                "the run is noted: {report:?}"
            );
            assert_eq!(rig.status().idle, None);
            let again = rig.pass();
            assert_eq!(again.failed, None, "{again:?}");
            assert_eq!(rig.status().pass, 2);
        }

        #[test]
        fn its_issue_is_not_claimed_and_does_not_end_the_pass() {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.max_runs = 2);
            unreadable_row(&rig, &RunId::new(REPO, 5, 1).unwrap());
            rig.hub.ready(5);
            rig.hub.ready(6);
            let report = rig.pass();
            assert_eq!(report.failed, None, "{report:?}");
            assert_eq!(rig.claimed_issues(), vec![6], "{report:?}");
            let reason = waiting_reason(&report, 5).unwrap_or_default();
            assert!(reason.contains("cannot be read"), "{reason}");
            assert!(
                rig.status().ready.iter().any(|w| w.issue == 5),
                "the ready queue lists it with the reason"
            );
        }

        #[test]
        fn it_holds_its_slot_and_is_not_counted_as_freed() {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.max_runs = 1);
            unreadable_row(&rig, &RunId::new(REPO, 5, 1).unwrap());
            rig.hub.ready(5);
            rig.hub.ready(6);
            let report = rig.pass();
            assert_eq!(report.failed, None, "{report:?}");
            assert!(rig.claimed_issues().is_empty(), "{report:?}");
            assert_eq!(waiting_reason(&report, 6).as_deref(), Some(NO_SLOT));
        }
    }

    mod a_ready_issue_whose_own_run_holds_the_last_slot {
        use super::*;

        /// The fleet's one run slot is held by run 5/1 of issue 5, which is ready again.
        fn rig_with(state: State, session: bool) -> (Rig, RunId) {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.max_runs = 1);
            let run = rig.run_in(5, 1, state);
            if session {
                rig.ledger
                    .write(
                        &run,
                        &Event::SessionLaunched {
                            thread_id: SessionId(uuid::Uuid::from_u128(0x56)).to_string(),
                            worktree: None,
                            branch: Some("fac/5-1".into()),
                            model_instance: None,
                            model: None,
                            model_options: None,
                            resumed_from: None,
                            base: Some("main".into()),
                            title: Some("Issue 5".into()),
                        },
                        &Cause::new(CauseStep::Start, "test"),
                    )
                    .unwrap();
            }
            rig.hub.ready(5);
            (rig, run)
        }

        /// A run in triage with no session ends with the claim, which takes its slot.
        #[test]
        fn is_claimed_in_the_slot_of_its_triage_run_that_has_no_session() {
            let (mut rig, _) = rig_with(State::Triage, false);
            let report = rig.pass();
            assert_eq!(report.failed, None, "{report:?}");
            assert_eq!(rig.claimed_issues(), vec![5], "{report:?}");
            assert_eq!(rig.book.runs_free(1), 0, "the claim holds the one slot");
        }

        /// A run with a session, or one past triage (here a building run with no session
        /// recorded, which the pass notes and leaves), keeps its slot: the issue waits.
        #[test]
        fn waits_while_its_run_has_a_session_or_is_building() {
            for (state, session) in [(State::Triage, true), (State::Building, false)] {
                let (mut rig, _) = rig_with(state, session);
                let report = rig.pass();
                assert!(
                    rig.claimed_issues().is_empty(),
                    "{state:?} {session}: {report:?}"
                );
                assert_eq!(
                    waiting_reason(&report, 5).as_deref(),
                    Some(NO_SLOT),
                    "{state:?} {session}: {report:?}"
                );
            }
        }
    }

    mod a_pass_that_panics {
        use super::*;

        /// A tracker that panics on every call, as a bad record or an answer of an unexpected
        /// shape does somewhere in a pass.
        struct Panics;

        impl crate::factory::github::Tracker for Panics {
            fn gh(&self, _: &[String]) -> Result<String, TrackerError> {
                panic!("an answer of a shape nobody expected");
            }
        }

        fn panicking(rig: &mut Rig) {
            rig.tracker = Hiding::new(Arc::new(Panics), Arc::new(|text: &str| text.to_string()));
        }

        #[test]
        fn is_counted_as_a_failed_pass_and_says_so() {
            let mut rig = Rig::new();
            panicking(&mut rig);
            let report = rig.pass();
            assert!(report.failed.is_some(), "{report:?}");
            assert_eq!(rig.memo.failures, 1);
            assert_eq!(rig.status().pass, 1, "a status is written");
        }

        #[test]
        fn three_in_a_row_make_the_status_idle() {
            let mut rig = Rig::new();
            panicking(&mut rig);
            for _ in 0..IDLE_AFTER_FAILURES {
                rig.pass();
            }
            let idle = rig.status().idle.unwrap_or_default();
            assert!(idle.contains("3 passes in a row failed"), "{idle}");
        }

        #[test]
        fn keeps_what_the_last_good_pass_found() {
            let mut rig = Rig::new();
            rig.hub.ready(5);
            rig.fleet_settings(|f| f.max_runs = 0);
            rig.pass();
            assert!(rig.memo.last.is_some());
            panicking(&mut rig);
            rig.pass();
            assert!(rig.memo.last.is_some(), "the memo is not reset");
            assert_eq!(rig.memo.failures, 1);
            assert_eq!(
                rig.status().ready.len(),
                1,
                "the last good results are written again"
            );
        }
    }

    mod the_finished_runs_of_the_status {
        use super::*;
        use crate::factory::ledger::events::Outcome;

        fn finish(rig: &Rig, run: &RunId) {
            rig.ledger
                .write(
                    run,
                    &Event::Finished {
                        outcome: Outcome::PrOpen,
                        total_ms: 0,
                        gate_attempts: 0,
                        review_cycles: None,
                        pr_url: None,
                        pr_end: None,
                    },
                    &Cause::new(CauseStep::Start, "test"),
                )
                .unwrap();
        }

        /// A run of `issue` that took `padding` more events and then finished.
        fn finished_run(rig: &Rig, issue: u64, padding: u32) -> RunId {
            let run = rig.run_in(issue, 1, State::Building);
            for n in 0..padding {
                rig.ledger
                    .write(
                        &run,
                        &Event::StepChanged {
                            step: format!("padding {n}"),
                        },
                        &Cause::new(CauseStep::Start, "test"),
                    )
                    .unwrap();
            }
            finish(rig, &run);
            run
        }

        #[test]
        fn are_newest_first_by_when_each_finished_and_not_by_issue_number() {
            let mut rig = Rig::new();
            for issue in 2..=21 {
                finished_run(&rig, issue, 0);
            }
            // Issue 1 is the lowest number and finishes last.
            finished_run(&rig, 1, 0);
            rig.pass();
            let issues: Vec<u64> = rig
                .status()
                .finished
                .iter()
                .map(|finished| finished.run.issue())
                .collect();
            let mut expected = vec![1];
            expected.extend((3..=21).rev());
            assert_eq!(issues, expected, "twenty kept, the oldest cut");
        }

        #[test]
        fn a_second_attempt_that_finishes_last_comes_first() {
            let mut rig = Rig::new();
            for issue in [3, 4] {
                finished_run(&rig, issue, 0);
            }
            let again = rig.run_in(3, 2, State::Building);
            finish(&rig, &again);
            rig.pass();
            let runs: Vec<RunId> = rig
                .status()
                .finished
                .iter()
                .map(|finished| finished.run.clone())
                .collect();
            assert_eq!(
                runs,
                vec![
                    again,
                    RunId::new(REPO, 4, 1).unwrap(),
                    RunId::new(REPO, 3, 1).unwrap()
                ]
            );
        }

        #[test]
        fn a_pass_over_an_unchanged_history_reads_no_finished_runs_events() {
            let mut rig = Rig::new();
            // Four runs of fifteen events each.
            for issue in 1..=4 {
                finished_run(&rig, issue, 10);
            }
            rig.pass();
            let read = Store::run_rows_read;
            let before = read();
            rig.pass();
            // Only the runs' ids are listed, a handful of times; no event of a finished run.
            assert!(read() - before <= 4 * 4, "{} rows", read() - before);
            assert_eq!(rig.status().finished.len(), 4);
        }

        #[test]
        fn a_pass_after_a_run_finishes_reads_the_events_of_that_run_and_not_the_seen_ones() {
            let mut rig = Rig::new();
            // Four runs of fifteen events each, seen by the first pass.
            for issue in 1..=4 {
                finished_run(&rig, issue, 10);
            }
            rig.pass();
            let new = finished_run(&rig, 5, 0);
            let its_events = rig.store.lock().unwrap().run_events(&new).unwrap().len();
            let read = Store::run_rows_read;
            let before = read();
            rig.pass();
            // The runs' ids, a handful of times, and the new run's events, a few times. Reading
            // even one seen run's fifteen events again goes over.
            let most = 5 * 4 + 3 * its_events;
            assert!(read() - before <= most, "{} rows", read() - before);
            assert_eq!(rig.status().finished.len(), 5);
        }

        #[test]
        fn a_run_that_finishes_later_joins_the_kept_ones_at_the_front() {
            let mut rig = Rig::new();
            finished_run(&rig, 1, 0);
            rig.pass();
            let live = rig.run_in(2, 1, State::Building);
            rig.hub.issue(2, &["factory:building"]);
            rig.pass();
            finish(&rig, &live);
            rig.pass();
            let issues: Vec<u64> = rig
                .status()
                .finished
                .iter()
                .map(|finished| finished.run.issue())
                .collect();
            assert_eq!(issues, vec![2, 1]);
        }

        #[test]
        fn the_sweep_reads_a_finished_run_once_not_twice() {
            let mut rig = Rig::new();
            for issue in 1..=3 {
                finished_run(&rig, issue, 10);
            }
            rig.pass();
            let read = Store::run_rows_read;
            let before = read();
            rig.pass();
            let plain = read() - before;
            rig.clock.advance(STUCK_READ_EVERY);
            let before = read();
            rig.pass();
            let swept = read() - before;
            // Three runs of fifteen events: the reconcile reads each once, and the sweep lists
            // the three ids; the pass does not read the events again for itself.
            let reconcile = 3 * 15;
            assert!(swept >= plain + reconcile, "{plain} {swept}");
            assert!(swept <= plain + reconcile + 3, "{plain} {swept}");
        }
    }

    mod the_held_up_runs_of_the_status {
        use super::*;
        use crate::factory::ledger::events::Outcome;

        /// Attempt `attempt` of `issue`, which went `to` (`stuck` or `needs-info`) saying
        /// `reason` and finished.
        fn held_up_run(rig: &Rig, issue: u64, attempt: u32, to: State, reason: &str) -> RunId {
            let run = rig.run_in(issue, attempt, State::Building);
            let cause = Cause::new(CauseStep::Start, "test");
            let (said, outcome) = if to == State::Stuck {
                let said = Event::Stuck {
                    reason: reason.into(),
                    branch: None,
                    commits_ahead: None,
                };
                (said, Outcome::Stuck)
            } else {
                let said = Event::QuestionsPosted {
                    questions: reason.into(),
                };
                (said, Outcome::NeedsInfo)
            };
            for event in [
                said,
                Event::StateChanged {
                    from: State::Building,
                    to,
                },
                Event::Finished {
                    outcome,
                    total_ms: 0,
                    gate_attempts: 0,
                    review_cycles: None,
                    pr_url: None,
                    pr_end: None,
                },
            ] {
                rig.ledger.write(&run, &event, &cause).unwrap();
            }
            run
        }

        #[test]
        fn a_run_finished_stuck_is_held_up_while_its_issue_is_stuck() {
            let mut rig = Rig::new();
            rig.hub.issue(5, &["factory:stuck"]);
            let run = held_up_run(&rig, 5, 1, State::Stuck, "the gate failed 3 times");
            rig.pass();
            assert_eq!(
                rig.status().held_up,
                vec![HeldUp {
                    run,
                    kind: HeldUpKind::Stuck,
                    reason: "the gate failed 3 times".into(),
                }]
            );
        }

        #[test]
        fn a_run_finished_needs_info_is_held_up_while_its_issue_needs_info() {
            let mut rig = Rig::new();
            rig.hub.issue(5, &["factory:needs-info"]);
            let run = held_up_run(&rig, 5, 1, State::NeedsInfo, "Which login?");
            rig.pass();
            assert_eq!(
                rig.status().held_up,
                vec![HeldUp {
                    run,
                    kind: HeldUpKind::NeedsInfo,
                    reason: "Which login?".into(),
                }]
            );
        }

        #[test]
        fn a_held_up_run_is_let_go_once_its_issue_leaves_the_label() {
            let mut rig = Rig::new();
            rig.hub.issue(5, &["factory:stuck"]);
            held_up_run(&rig, 5, 1, State::Stuck, "no");
            rig.pass();
            assert_eq!(rig.status().held_up.len(), 1);
            rig.hub.edit(5, |issue| issue.labels.clear());
            rig.pass();
            assert!(rig.status().held_up.is_empty());
        }

        /// The label is the newest run's: an earlier attempt that went stuck is not held up
        /// again, nor a run whose outcome is not the label the issue carries.
        #[test]
        fn only_the_newest_run_of_the_issue_is_held_up_and_only_for_its_own_label() {
            let mut rig = Rig::new();
            rig.hub.issue(5, &["factory:stuck"]);
            held_up_run(&rig, 5, 1, State::Stuck, "the first time");
            let newest = held_up_run(&rig, 5, 2, State::Stuck, "the second time");
            rig.hub.issue(6, &["factory:stuck"]);
            held_up_run(&rig, 6, 1, State::NeedsInfo, "Which login?");
            rig.pass();
            let held: Vec<RunId> = rig.status().held_up.into_iter().map(|h| h.run).collect();
            assert_eq!(held, vec![newest]);
        }
    }

    mod feedback_and_the_fleets_slots {
        use super::*;

        /// A tracker that says how many run slots the fleet has free while the feedback of a
        /// pull request is read, and finds none.
        struct Watching {
            inner: Arc<crate::factory::pass::testing::Hub>,
            book: Arc<Book>,
            free_during_reads: Mutex<Vec<u32>>,
            /// What the read finds.
            found: Vec<crate::factory::github::Feedback>,
        }

        impl crate::factory::github::Tracker for Watching {
            fn gh(&self, args: &[String]) -> Result<String, TrackerError> {
                // The hub does not know a pull request's draft state.
                if args.first().map(String::as_str) == Some("pr")
                    && args.get(1).map(String::as_str) == Some("ready")
                {
                    return Ok(String::new());
                }
                self.inner.gh(args)
            }

            fn pull_request(
                &self,
                _: &str,
                number: u64,
            ) -> Result<Option<crate::factory::github::PullRequest>, TrackerError> {
                Ok(Some(crate::factory::github::PullRequest {
                    number,
                    is_draft: false,
                    open: true,
                    merged: false,
                    head_ref: "fac/3-1".into(),
                    cross_repository: false,
                    base_ref: "main".into(),
                    head_sha: SHA.into(),
                    mergeable: crate::factory::github::Mergeable::Mergeable,
                    created_at: "2026-10-29T05:50:00Z".into(),
                }))
            }

            fn pr_feedback(
                &self,
                _: &str,
                _: u64,
                _: &str,
                _: &str,
                _: &[String],
            ) -> Result<Vec<crate::factory::github::Feedback>, TrackerError> {
                let free = self.book.runs_free(1);
                self.free_during_reads.lock().unwrap().push(free);
                Ok(self.found.clone())
            }

            fn activity_since(
                &self,
                _: &str,
                _: u64,
                _: &str,
                _: &[String],
            ) -> Result<crate::factory::github::Activity, TrackerError> {
                Ok(crate::factory::github::Activity {
                    comments: Vec::new(),
                    body_edit: None,
                    dropped: Vec::new(),
                })
            }
        }

        impl Commenting for Watching {
            fn hide(&self, text: &str) -> String {
                text.to_owned()
            }
        }

        /// A run of issue 3 whose pull request is open and whose session ran.
        fn in_pr_open(rig: &Rig) -> RunId {
            let run = rig.run_in(3, 1, State::Building);
            let cause = Cause::new(CauseStep::Start, "test");
            let write = |event: Event| rig.ledger.write(&run, &event, &cause).unwrap();
            write(Event::SessionLaunched {
                thread_id: "t-1".into(),
                worktree: Some(WorkspaceId(uuid::Uuid::from_u128(0x44)).to_string()),
                branch: Some("fac/3-1".into()),
                model_instance: None,
                model: None,
                model_options: None,
                resumed_from: None,
                base: Some("main".into()),
                title: Some("Issue 3".into()),
            });
            write(Event::PrOpened {
                pr: 12,
                url: format!("https://github.com/{REPO}/pull/12"),
            });
            write(Event::StateChanged {
                from: State::Building,
                to: State::PrOpen,
            });
            rig.hub.issue(3, &["factory:pr-open"]);
            run
        }

        #[test]
        fn a_read_that_finds_nothing_holds_no_slot_while_it_waits_on_github() {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.max_runs = 1);
            in_pr_open(&rig);
            let book = Arc::new(Book::default());
            let watching = Arc::new(Watching {
                inner: rig.hub.clone(),
                book: book.clone(),
                free_during_reads: Mutex::new(Vec::new()),
                found: Vec::new(),
            });
            let world = World {
                project: &rig.project,
                lease: &rig.lease,
                guard: rig.guard.as_ref(),
                tracker: &*watching,
                worker: &rig.worker,
                view: &rig.view,
                ledger: &rig.ledger,
                store: &rig.store,
                me: me(),
                shared: &rig.shared,
                book: &book,
                clock: &rig.clock,
            };
            let report = run(&world, &mut rig.memo);
            assert_eq!(report.failed, None, "{report:?}");
            let free = watching.free_during_reads.lock().unwrap().clone();
            assert!(!free.is_empty(), "the feedback was read: {report:?}");
            assert!(
                free.iter().all(|free| *free == 1),
                "the one slot was taken during a read that started no round: {free:?}"
            );
            assert_eq!(book.runs_free(1), 1);
        }

        fn review() -> crate::factory::github::Feedback {
            crate::factory::github::Feedback {
                id: "pr-comment:9".into(),
                author: "tom".into(),
                kind: crate::factory::github::FeedbackKind::PrComment,
                created_at: "2099-01-01T00:00:00Z".into(),
                body: "please rename it".into(),
                inline: Vec::new(),
                inline_unread: 0,
            }
        }

        /// One pass of `rig` over a tracker that finds `found`, with `book` as the fleet's.
        fn pass_finding(
            rig: &mut Rig,
            book: &Arc<Book>,
            found: Vec<crate::factory::github::Feedback>,
        ) -> PassReport {
            let watching = Arc::new(Watching {
                inner: rig.hub.clone(),
                book: book.clone(),
                free_during_reads: Mutex::new(Vec::new()),
                found,
            });
            let world = World {
                project: &rig.project,
                lease: &rig.lease,
                guard: rig.guard.as_ref(),
                tracker: &*watching,
                worker: &rig.worker,
                view: &rig.view,
                ledger: &rig.ledger,
                store: &rig.store,
                me: me(),
                shared: &rig.shared,
                book,
                clock: &rig.clock,
            };
            run(&world, &mut rig.memo)
        }

        #[test]
        fn feedback_that_is_found_with_a_slot_free_starts_its_round_and_the_run_holds_the_slot() {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.max_runs = 1);
            let run = in_pr_open(&rig);
            let book = Arc::new(Book::default());
            let report = pass_finding(&mut rig, &book, vec![review()]);
            assert_eq!(report.failed, None, "{report:?}");
            let said = report
                .runs
                .iter()
                .filter(|(r, _)| *r == run)
                .map(|(_, said)| said.clone())
                .collect::<Vec<_>>()
                .join(" | ");
            assert!(said.contains("Decided"), "{said}");
            assert_eq!(book.runs_free(1), 0, "the round holds the one slot");
        }

        #[test]
        fn feedback_that_is_found_with_no_slot_free_waits_and_takes_nothing() {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.max_runs = 1);
            in_pr_open(&rig);
            let book = Arc::new(Book::default());
            let other = book.ticket(other_project());
            book.begin(
                other,
                Held {
                    runs: 1,
                    gates: 0,
                    placed: vec![],
                },
            );
            let report = pass_finding(&mut rig, &book, vec![review()]);
            assert_eq!(report.failed, None, "{report:?}");
            let waiting = &rig.status().feedback_waiting;
            assert_eq!(waiting.len(), 1, "{report:?}");
            assert_eq!(waiting[0].reason, NO_SLOT);
            assert_eq!(
                book.runs_free(1),
                0,
                "only the other project's slot is held"
            );
        }
    }

    mod the_book_follows_the_ledger {
        use super::*;
        use crate::factory::github::{Commenting, Tracker};

        /// The hub as a tracker whose status comment cannot be written, as a `gh` that fails
        /// after the claim has landed does.
        struct StatusFails(Arc<crate::factory::pass::testing::Hub>);

        impl Tracker for StatusFails {
            fn gh(&self, args: &[String]) -> Result<String, TrackerError> {
                self.0.gh(args)
            }
        }

        impl Commenting for StatusFails {
            fn hide(&self, text: &str) -> String {
                text.to_owned()
            }

            fn upsert_status_comment(
                &self,
                _: &str,
                _: u64,
                _: &str,
                _: &str,
            ) -> Result<(), TrackerError> {
                Err(TrackerError::Other {
                    kind: crate::factory::github::OtherKind::Failed,
                    message: "the status comment could not be written".into(),
                })
            }
        }

        fn pass_over(rig: &mut Rig, tracker: &dyn Commenting, book: &Book) -> PassReport {
            let world = World {
                project: &rig.project,
                lease: &rig.lease,
                guard: rig.guard.as_ref(),
                tracker,
                worker: &rig.worker,
                view: &rig.view,
                ledger: &rig.ledger,
                store: &rig.store,
                me: me(),
                shared: &rig.shared,
                book,
                clock: &rig.clock,
            };
            run(&world, &mut rig.memo)
        }

        #[test]
        fn a_run_carried_on_from_its_claim_holds_a_slot_before_the_claims_read_them() {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.max_runs = 1);
            let run = RunId::new(REPO, 5, 1).unwrap();
            rig.hub.issue(5, &["factory:triage"]);
            rig.hub
                .edit(5, |issue| issue.assignees = vec![LOGIN.into()]);
            rig.hub.comment_as(
                5,
                LOGIN,
                &crate::factory::github::claim_comment(&run, &builder_machine()),
            );
            rig.hub.ready(6);
            let report = rig.pass();
            assert!(rig.claimed_issues().is_empty(), "{report:?}");
            assert_eq!(waiting_reason(&report, 6).as_deref(), Some(NO_SLOT));
            assert_eq!(rig.book.runs_free(1), 0, "the carried run holds the slot");
        }

        #[test]
        fn a_run_carried_on_and_a_claim_both_fit_when_two_slots_are_free() {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.max_runs = 2);
            let run = RunId::new(REPO, 5, 1).unwrap();
            rig.hub.issue(5, &["factory:triage"]);
            rig.hub
                .edit(5, |issue| issue.assignees = vec![LOGIN.into()]);
            rig.hub.comment_as(
                5,
                LOGIN,
                &crate::factory::github::claim_comment(&run, &builder_machine()),
            );
            rig.hub.ready(6);
            let report = rig.pass();
            assert_eq!(rig.claimed_issues(), vec![6], "{report:?}");
            assert_eq!(rig.book.runs_free(2), 0, "both runs hold a slot");
        }

        #[test]
        fn a_claim_that_errors_after_it_recorded_keeps_its_slot() {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.max_runs = 1);
            rig.hub.ready(5);
            let book = Book::default();
            let tracker = StatusFails(rig.hub.clone());
            let report = pass_over(&mut rig, &tracker, &book);
            assert!(report.failed.is_some(), "{report:?}");
            assert!(
                !rig.ledger
                    .read(&RunId::new(REPO, 5, 1).unwrap())
                    .unwrap()
                    .is_empty(),
                "the claim was recorded"
            );
            assert_eq!(
                book.runs_free(1),
                0,
                "another project's pass must not take the slot of a run that is going"
            );
        }
    }

    mod seeding_the_book {
        use super::*;

        #[test]
        fn a_project_whose_pass_has_not_begun_counts_what_its_ledger_holds_once_seeded() {
            let rig = Rig::new();
            rig.run_in(3, 1, State::Building);
            let book = Book::default();
            seed(&book, &rig.ledger, project_id(), REPO).unwrap();
            assert_eq!(book.runs_free(3), 2);
        }

        #[test]
        fn seeding_again_counts_the_runs_once() {
            let rig = Rig::new();
            rig.run_in(3, 1, State::Building);
            let book = Book::default();
            seed(&book, &rig.ledger, project_id(), REPO).unwrap();
            seed(&book, &rig.ledger, project_id(), REPO).unwrap();
            assert_eq!(book.runs_free(3), 2);
        }

        #[test]
        fn a_pass_begun_before_the_seed_changes_nothing_after_it() {
            let rig = Rig::new();
            rig.run_in(3, 1, State::Building);
            let book = Book::default();
            let stale = book.ticket(project_id());
            seed(&book, &rig.ledger, project_id(), REPO).unwrap();
            book.finish(stale, Held::default());
            assert!(!book.take_run(stale, 9));
            assert_eq!(book.runs_free(3), 2, "the seed stands");
        }

        #[test]
        fn a_project_forgotten_holds_nothing_even_when_it_was_seeded() {
            let rig = Rig::new();
            rig.run_in(3, 1, State::Building);
            let book = Book::default();
            seed(&book, &rig.ledger, project_id(), REPO).unwrap();
            book.forget(project_id());
            assert_eq!(book.runs_free(3), 3);
        }
    }

    mod a_failed_claim_keeps_what_the_pass_reserved {
        use super::*;

        /// Every write of a run event of `issue` that names a state change fails from now on, as
        /// a store that fails after the claim recorded its run does.
        fn fail_state_change_of(rig: &Rig, issue: u64) {
            let conn = rusqlite::Connection::open(rig._dir.path().join("foundry.db")).unwrap();
            conn.execute_batch(&format!(
                "CREATE TRIGGER fail_issue_{issue} BEFORE INSERT ON factory_records \
                 WHEN NEW.kind = 'run_event' AND NEW.id LIKE 'rawphp__softfac__{issue}__%' \
                 AND NEW.record LIKE '%state_changed%' \
                 BEGIN SELECT RAISE(ABORT, 'the write failed'); END;"
            ))
            .unwrap();
        }

        #[test]
        fn a_claim_placed_and_started_earlier_still_holds_its_machine_after_a_later_claim_fails() {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.max_runs = 5);
            // Only the builder takes a session, and it takes two.
            rig.shared.limits.insert(builder_machine(), limits(2));
            for issue in [5, 6, 7] {
                rig.hub.ready(issue);
            }
            // The fleet shows the workspace of 5 ready, so its session starts in the claim's own
            // advance: the run holds no machine reservation in the ledger from then on.
            let terminal = TerminalId(uuid::Uuid::from_u128(0x78));
            rig.view = FleetView::builder()
                .online(me())
                .online(builder_machine())
                .sample(sample(builder_machine(), 0))
                .held(
                    builder_machine(),
                    MachineHeld {
                        logins: vec![LOGIN.into()],
                        skill: true,
                    },
                )
                .workspace(Record {
                    owner: builder_machine(),
                    version: 1,
                    deleted: false,
                    body: Workspace {
                        id: WorkspaceId(uuid::Uuid::from_u128(0x77)),
                        project: project_id(),
                        name: "fac/5-1".into(),
                        branch: "fac/5-1".into(),
                        base: "main".into(),
                        setup: Some(terminal),
                    },
                })
                .terminal(Record {
                    owner: builder_machine(),
                    version: 1,
                    deleted: false,
                    body: foundry_proto::record::Terminal {
                        id: terminal,
                        workspace: WorkspaceId(uuid::Uuid::from_u128(0x77)),
                        state: foundry_proto::state::TerminalState::Exited,
                        exit_code: Some(0),
                    },
                })
                .build();
            fail_state_change_of(&rig, 6);
            let report = rig.pass();
            assert_eq!(report.failed, None, "{report:?}");
            assert!(
                rig.ledger
                    .read(&RunId::new(REPO, 6, 1).unwrap())
                    .is_ok_and(|events| !events.is_empty()),
                "the claim of 6 recorded its run before it failed: {report:?}"
            );
            assert!(
                report
                    .runs
                    .iter()
                    .any(|(run, said)| run.issue() == 5 && said.contains("session started")),
                "the session of 5 started: {report:?}"
            );
            assert!(
                !rig.claimed_issues().contains(&7),
                "5 and 6 hold the builder's two sessions: {report:?}"
            );
            let reason = waiting_reason(&report, 7).unwrap_or_default();
            assert!(reason.contains("no machine"), "{reason}");
        }

        #[test]
        fn a_claim_that_fails_after_it_recorded_keeps_its_slot_against_the_next_ready_issue() {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.max_runs = 1);
            rig.hub.ready(5);
            rig.hub.ready(6);
            fail_state_change_of(&rig, 5);
            let report = rig.pass();
            assert_eq!(report.failed, None, "{report:?}");
            assert_eq!(rig.claimed_issues(), vec![5], "{report:?}");
            assert_eq!(waiting_reason(&report, 6).as_deref(), Some(NO_SLOT));
        }
    }

    mod a_pass_that_panics_after_a_carried_run_recorded {
        use super::*;
        use crate::factory::github::Tracker;

        /// The hub, but every call made once `run` has its `claimed` event panics, as an answer
        /// of a shape nobody expected does in the middle of a step.
        struct PanicsOnceClaimed {
            hub: Arc<Hub>,
            store: Arc<Mutex<Store>>,
            run: RunId,
        }

        impl Tracker for PanicsOnceClaimed {
            fn gh(&self, args: &[String]) -> Result<String, TrackerError> {
                let claimed = self
                    .store
                    .lock()
                    .unwrap()
                    .run_has_event(&self.run, "claimed")
                    .unwrap();
                if claimed {
                    panic!("an answer of a shape nobody expected");
                }
                self.hub.gh(args)
            }
        }

        #[test]
        fn leaves_the_slot_of_the_run_it_recorded_held_in_the_book() {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.max_runs = 1);
            let run = RunId::new(REPO, 5, 1).unwrap();
            rig.hub.issue(5, &["factory:triage"]);
            rig.hub
                .edit(5, |issue| issue.assignees = vec![LOGIN.into()]);
            rig.hub.comment_as(
                5,
                LOGIN,
                &crate::factory::github::claim_comment(&run, &builder_machine()),
            );
            rig.tracker = Hiding::new(
                Arc::new(PanicsOnceClaimed {
                    hub: rig.hub.clone(),
                    store: rig.store.clone(),
                    run: run.clone(),
                }),
                Arc::new(|text: &str| text.to_string()),
            );
            let report = rig.pass();
            assert_eq!(
                report.failed.as_deref(),
                Some("the pass panicked"),
                "{report:?}"
            );
            assert!(
                !rig.ledger.read(&run).unwrap().is_empty(),
                "the takeover recorded its run before the panic"
            );
            assert_eq!(
                rig.book.runs_free(1),
                0,
                "no other project's pass may take the slot of a run that is going"
            );
        }
    }

    mod a_finished_run_this_build_cannot_fully_read {
        use super::a_run_this_build_cannot_read::unreadable_row;
        use super::*;
        use crate::factory::ledger::events::Outcome;

        #[test]
        fn is_noted_by_the_sweep_and_does_not_fail_the_pass() {
            let mut rig = Rig::new();
            let run = rig.run_in(5, 1, State::Building);
            rig.ledger
                .write(
                    &run,
                    &Event::Finished {
                        outcome: Outcome::PrOpen,
                        total_ms: 0,
                        gate_attempts: 0,
                        review_cycles: None,
                        pr_url: None,
                        pr_end: None,
                    },
                    &Cause::new(CauseStep::Start, "test"),
                )
                .unwrap();
            unreadable_row(&rig, &run);
            let report = rig.pass();
            assert_eq!(report.failed, None, "{report:?}");
            assert!(
                report.runs.contains(&(run, UNREADABLE_SWEPT.into())),
                "the run is noted: {report:?}"
            );
        }
    }

    mod the_finished_runs_across_a_change_of_lease_holder {
        use super::*;
        use crate::factory::ledger::events::Outcome;

        #[test]
        fn a_run_finished_at_a_higher_generation_comes_first_whatever_its_version() {
            let mut rig = Rig::new();
            let finished = Event::Finished {
                outcome: Outcome::PrOpen,
                total_ms: 0,
                gate_attempts: 0,
                review_cycles: None,
                pr_url: None,
                pr_end: None,
            };
            let cause = Cause::new(CauseStep::Start, "test");
            // This machine finishes run 3 late in its own versions, at generation 1.
            let first = rig.run_in(3, 1, State::Building);
            for n in 0..20 {
                rig.ledger
                    .write(
                        &first,
                        &Event::StepChanged {
                            step: format!("padding {n}"),
                        },
                        &cause,
                    )
                    .unwrap();
            }
            rig.ledger.write(&first, &finished, &cause).unwrap();
            // The lease passes to another machine, whose versions start low and whose
            // generation is higher: it finishes run 4 last.
            let second = rig.run_in(4, 1, State::Building);
            let taker_id = MachineId(uuid::Uuid::from_u128(0x3));
            let (_taker_dir, taker_store) = open_store();
            let taker = Ledger::new(
                taker_store.clone(),
                taker_id,
                2,
                crate::factory::ledger::testing::shown(),
            );
            taker.write(&second, &finished, &cause).unwrap();
            // Its record reaches this machine as a synced one does.
            let synced = taker_store
                .lock()
                .unwrap()
                .factory_since(taker_id, 0, 10)
                .unwrap();
            assert_eq!(synced.len(), 1);
            assert!(
                synced[0].version < 5,
                "a low version: {}",
                synced[0].version
            );
            rig.store
                .lock()
                .unwrap()
                .apply_factory(&synced[0], taker_id)
                .unwrap();
            rig.pass();
            let runs: Vec<RunId> = rig
                .status()
                .finished
                .iter()
                .map(|finished| finished.run.clone())
                .collect();
            assert_eq!(runs, vec![second, first]);
        }
    }

    mod the_sweep_when_the_store_fails {
        use super::*;
        use crate::factory::ledger::events::Outcome;

        fn finish(rig: &Rig, run: &RunId) {
            rig.ledger
                .write(
                    run,
                    &Event::Finished {
                        outcome: Outcome::PrOpen,
                        total_ms: 0,
                        gate_attempts: 0,
                        review_cycles: None,
                        pr_url: None,
                        pr_end: None,
                    },
                    &Cause::new(CauseStep::Start, "test"),
                )
                .unwrap();
        }

        /// A finished run of `issue`.
        pub(super) fn finished(rig: &Rig, issue: u64) -> RunId {
            let run = rig.run_in(issue, 1, State::Building);
            finish(rig, &run);
            run
        }

        pub(super) fn workspace_of(issue: u64) -> WorkspaceId {
            WorkspaceId(uuid::Uuid::from_u128(0xa00 + u128::from(issue)))
        }

        /// The fleet with the build workspace of each of `issues` live on the builder.
        pub(super) fn fleet_with(issues: &[u64]) -> FleetView {
            let mut view = FleetView::builder().online(me()).online(builder_machine());
            for issue in issues {
                view = view.workspace(Record {
                    owner: builder_machine(),
                    version: 1,
                    deleted: false,
                    body: Workspace {
                        id: workspace_of(*issue),
                        project: project_id(),
                        name: format!("fac/{issue}-1"),
                        branch: format!("fac/{issue}-1"),
                        base: "main".into(),
                        setup: None,
                    },
                });
            }
            view.build()
        }

        /// From the archive of `issue`'s workspace on, every read of the store fails with an
        /// error that is not a decode error: the table is gone, as a store that fails does.
        fn break_store_when_archived(rig: &Rig, issue: u64) {
            break_store_with(rig, issue, "ALTER TABLE factory_records RENAME TO gone");
        }

        /// The same, with `sql` run on the database as the store's failure.
        pub(super) fn break_store_with(rig: &Rig, issue: u64, sql: &'static str) {
            let path = rig._dir.path().join("foundry.db");
            *rig.worker.after_archive.lock().unwrap() = Some(Box::new(move |archived| {
                if archived == workspace_of(issue) {
                    rusqlite::Connection::open(&path)
                        .unwrap()
                        .execute_batch(sql)
                        .unwrap();
                }
            }));
        }

        fn unreadable_notes(report: &PassReport) -> Vec<&RunId> {
            report
                .runs
                .iter()
                .filter(|(_, said)| said == UNREADABLE_SWEPT)
                .map(|(run, _)| run)
                .collect()
        }

        /// Run 3 and run 4 are finished and seen so by a first pass. Then run 3's workspace
        /// is live again, the sweep is due, and the store fails once run 3's is archived.
        #[test]
        fn a_reconcile_that_fails_in_the_store_ends_the_pass_and_notes_no_run() {
            let mut rig = Rig::new();
            let (first, second) = (finished(&rig, 3), finished(&rig, 4));
            assert_eq!(rig.pass().failed, None);
            rig.view = fleet_with(&[3]);
            rig.clock.advance(STUCK_READ_EVERY);
            break_store_when_archived(&rig, 3);
            let report = rig.pass();
            assert!(report.failed.is_some(), "{report:?}");
            assert!(
                report.archived.contains(&workspace_of(3)),
                "run {first}'s workspace was archived before the store failed: {report:?}"
            );
            assert_eq!(
                unreadable_notes(&report),
                Vec::<&RunId>::new(),
                "run {second} is readable: only the store failed: {report:?}"
            );
        }

        /// Run 4 finished after the first pass, so the memo does not know it: asking the store
        /// whether it finished is the first read to fail. A failed question is not an answer of
        /// "not finished": the sweep did not reach run 4, and the pass says so.
        #[test]
        fn a_failed_check_for_a_finished_run_ends_the_pass() {
            let mut rig = Rig::new();
            finished(&rig, 3);
            assert_eq!(rig.pass().failed, None);
            let second = finished(&rig, 4);
            rig.view = fleet_with(&[3]);
            rig.clock.advance(STUCK_READ_EVERY);
            break_store_with(&rig, 3, "ALTER TABLE factory_records DROP COLUMN record");
            let report = rig.pass();
            assert!(
                report.failed.is_some(),
                "the sweep could not ask whether {second} finished: {report:?}"
            );
            assert_eq!(
                unreadable_notes(&report),
                Vec::<&RunId>::new(),
                "{report:?}"
            );
        }

        /// The store fails after run 3's reconcile, and the orphan read of the same run is the
        /// first read to find it.
        #[test]
        fn an_orphan_read_that_fails_in_the_store_ends_the_pass_and_notes_no_run() {
            let mut rig = Rig::new();
            let run = finished(&rig, 3);
            rig.view = fleet_with(&[3]);
            break_store_when_archived(&rig, 3);
            let report = rig.pass();
            assert!(report.failed.is_some(), "{report:?}");
            assert!(
                report.archived.contains(&workspace_of(3)),
                "the reconcile archived it before the store failed: {report:?}"
            );
            assert_eq!(
                unreadable_notes(&report),
                Vec::<&RunId>::new(),
                "run {run} is readable: only the store failed: {report:?}"
            );
        }
    }

    mod a_pass_that_fails_after_a_carried_run_recorded {
        use super::*;
        use crate::factory::github::{OtherKind, Tracker};

        /// The hub, but every call made once `run` has its `claimed` event is answered with an
        /// error, as a GitHub that stops answering in the middle of a step does.
        struct FailsOnceClaimed {
            hub: Arc<Hub>,
            store: Arc<Mutex<Store>>,
            run: RunId,
        }

        impl Tracker for FailsOnceClaimed {
            fn gh(&self, args: &[String]) -> Result<String, TrackerError> {
                let claimed = self
                    .store
                    .lock()
                    .unwrap()
                    .run_has_event(&self.run, "claimed")
                    .unwrap();
                if claimed {
                    return Err(TrackerError::Other {
                        kind: OtherKind::Failed,
                        message: "gh stopped answering".into(),
                    });
                }
                self.hub.gh(args)
            }
        }

        #[test]
        fn leaves_the_slot_of_the_run_it_recorded_held_in_the_book() {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.max_runs = 1);
            let run = RunId::new(REPO, 5, 1).unwrap();
            rig.hub.issue(5, &["factory:triage"]);
            rig.hub
                .edit(5, |issue| issue.assignees = vec![LOGIN.into()]);
            rig.hub.comment_as(
                5,
                LOGIN,
                &crate::factory::github::claim_comment(&run, &builder_machine()),
            );
            rig.tracker = Hiding::new(
                Arc::new(FailsOnceClaimed {
                    hub: rig.hub.clone(),
                    store: rig.store.clone(),
                    run: run.clone(),
                }),
                Arc::new(|text: &str| text.to_string()),
            );
            let report = rig.pass();
            assert!(report.failed.is_some(), "{report:?}");
            assert!(
                !rig.ledger.read(&run).unwrap().is_empty(),
                "the takeover recorded its run before the call failed"
            );
            assert_eq!(
                rig.book.runs_free(1),
                0,
                "no other project's pass may take the slot of a run that is going"
            );
        }
    }

    mod a_run_placed_when_the_pass_begins {
        use super::*;

        /// A run of issue 3 is placed on the builder (two sessions), its session not started.
        /// A claim of issue 5 is carried on, and then the next ready issue is claimed: the
        /// builder holds one session for run 3 and one is free for issue 6, however many
        /// times the pass sets the book from the ledger.
        #[test]
        fn is_reserved_on_its_machine_once_however_often_the_pass_counts_it() {
            let mut rig = Rig::new();
            rig.fleet_settings(|f| f.max_runs = 5);
            rig.shared.limits.insert(builder_machine(), limits(2));
            rig.hub.issue(3, &["factory:triage"]);
            rig.run_in(3, 1, State::Triage);
            let carried = RunId::new(REPO, 5, 1).unwrap();
            rig.hub.issue(5, &["factory:triage"]);
            rig.hub
                .edit(5, |issue| issue.assignees = vec![LOGIN.into()]);
            rig.hub.comment_as(
                5,
                LOGIN,
                &crate::factory::github::claim_comment(&carried, &me()),
            );
            rig.hub.ready(6);
            let report = rig.pass();
            assert_eq!(report.failed, None, "{report:?}");
            assert_eq!(rig.claimed_issues(), vec![6], "{report:?}");
        }
    }

    mod the_sweep_that_ends_part_way {
        use super::the_sweep_when_the_store_fails::{
            break_store_with, finished, fleet_with, workspace_of,
        };
        use super::*;

        /// The store fails in the sweep after it began: the pass fails, and the next pass sweeps
        /// again, so that the finished runs the sweep did not reach do not wait for another
        /// STUCK_READ_EVERY.
        #[test]
        fn is_swept_again_by_the_next_pass() {
            let mut rig = Rig::new();
            finished(&rig, 3);
            finished(&rig, 4);
            assert_eq!(rig.pass().failed, None);
            let swept = rig.memo.swept;
            assert!(swept.is_some());
            rig.view = fleet_with(&[3]);
            rig.clock.advance(STUCK_READ_EVERY);
            // The sweep is due, and the store fails once run 3's workspace is archived.
            break_store_with(&rig, 3, "ALTER TABLE factory_records RENAME TO gone");
            let report = rig.pass();
            assert!(report.failed.is_some(), "{report:?}");
            assert!(report.archived.contains(&workspace_of(3)), "{report:?}");
            assert_eq!(
                rig.memo.swept, swept,
                "a sweep that did not finish is not counted as done"
            );
        }

        /// A run of `issue` that finished stuck.
        fn finished_stuck(rig: &Rig, issue: u64) -> RunId {
            let run = rig.run_in(issue, 1, State::Stuck);
            rig.ledger
                .write(
                    &run,
                    &Event::Finished {
                        outcome: crate::factory::ledger::events::Outcome::Stuck,
                        total_ms: 0,
                        gate_attempts: 0,
                        review_cycles: None,
                        pr_url: None,
                        pr_end: None,
                    },
                    &Cause::new(CauseStep::Start, "test"),
                )
                .unwrap();
            run
        }

        /// A finished stuck run whose issue GitHub refuses to show every time: the pass that
        /// sweeps fails, and the next one sweeps on from the run after it, so one run cannot
        /// fail every pass of its project.
        #[test]
        fn a_sweep_that_stops_at_its_last_run_is_done_at_the_next_pass() {
            let mut rig = Rig::new();
            finished_stuck(&rig, 3);
            assert_eq!(rig.pass().failed, None);
            rig.view = fleet_with(&[3]);
            rig.hub.fail("issue view 3", usize::MAX);
            rig.clock.advance(STUCK_READ_EVERY);
            let first = rig.pass();
            assert!(first.failed.is_some(), "the sweep is due: {first:?}");
            let next = rig.pass();
            assert_eq!(next.failed, None, "it sweeps on after run 3: {next:?}");
            let after = rig.pass();
            assert_eq!(after.failed, None, "{after:?}");
            assert_eq!(rig.status().idle, None, "{:?}", rig.status());
        }

        /// Runs 3, 4 and 5 finished stuck, and GitHub refuses to show any of their issues. The
        /// sweep stops at run 3, and its one retry stops at run 4: that retry counts as the
        /// sweep, so the pass after it does not sweep and ends well, and three failed passes in
        /// a row never make the project idle.
        #[test]
        fn a_sweep_that_fails_again_waits_for_the_next_stuck_read() {
            let mut rig = Rig::new();
            for issue in [3, 4, 5] {
                finished_stuck(&rig, issue);
            }
            assert_eq!(rig.pass().failed, None);
            rig.view = fleet_with(&[3, 4, 5]);
            for issue in [3, 4, 5] {
                rig.hub.fail(&format!("issue view {issue}"), usize::MAX);
            }
            rig.clock.advance(STUCK_READ_EVERY);
            let first = rig.pass();
            assert!(
                first.failed.is_some(),
                "the sweep stops at run 3: {first:?}"
            );
            let retry = rig.pass();
            assert!(
                retry.failed.is_some(),
                "the retry stops at run 4: {retry:?}"
            );
            let next = rig.pass();
            assert_eq!(
                next.failed, None,
                "the next sweep waits for STUCK_READ_EVERY: {next:?}"
            );
            assert_eq!(rig.status().idle, None, "{:?}", rig.status());
        }

        /// Runs 3 and 4 finished stuck and keep a workspace each. Issue 3 cannot be read, and
        /// issue 4 has left `factory:stuck`: the sweep that stops at run 3 goes on from run 4
        /// at the next pass, and archives run 4's workspace. Run 3 is tried again at the
        /// next sweep.
        #[test]
        fn a_run_whose_sweep_always_fails_does_not_keep_the_runs_after_it_from_being_swept() {
            let mut rig = Rig::new();
            finished_stuck(&rig, 3);
            finished_stuck(&rig, 4);
            rig.hub.issue(4, &[]);
            assert_eq!(rig.pass().failed, None);
            rig.view = fleet_with(&[3, 4]);
            rig.hub.fail("issue view 3", usize::MAX);
            rig.clock.advance(STUCK_READ_EVERY);
            let first = rig.pass();
            assert!(
                first.failed.is_some(),
                "the sweep stops at run 3: {first:?}"
            );
            assert!(!first.archived.contains(&workspace_of(4)), "{first:?}");
            let next = rig.pass();
            assert_eq!(next.failed, None, "{next:?}");
            assert!(
                next.archived.contains(&workspace_of(4)),
                "the sweep goes on from run 4: {next:?}"
            );
            rig.clock.advance(STUCK_READ_EVERY);
            let again = rig.pass();
            assert!(
                again.failed.is_some(),
                "the next sweep tries run 3 again: {again:?}"
            );
        }
    }
}

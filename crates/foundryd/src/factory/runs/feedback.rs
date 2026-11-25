//! Gate failures and instructor comments sent back to a run's session.

use foundry_proto::factory::RunId;
use foundry_proto::ids::SessionId;
use foundry_proto::settings::SharedSettings;

use super::{RunContext, RunError};
use crate::factory::placing::{Reserved, Slots};

/// What a pass of [`advance`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeedbackStep {
    /// Nothing to do: nothing was written or asked of a worker.
    None,
    /// New feedback is waiting and no round can start now.
    Waiting { reason: String },
    /// The round was decided: the label is `building` and the earlier workspace's archive was
    /// asked for if one shows.
    Decided { round: u32, pr: u64, items: usize },
    /// A round under way waits for something.
    Preparing(Wait),
    /// The round's session runs.
    Started { round: u32, session: SessionId },
    /// The first message was asked for again by count.
    Resent { session: SessionId },
    /// The run went stuck.
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
    /// The create was answered and the fleet does not show the record yet.
    Workspace,
    /// The setup terminal runs.
    Setup,
}

/// Why a round that has feedback does not start when every run slot is held.
pub const NO_SLOT: &str = "no run slot is free";

/// One pass of the feedback step over `run`.
pub fn advance(
    ctx: &RunContext<'_>,
    run: &RunId,
    shared: &SharedSettings,
    slots: &mut Slots,
    reserved: &mut Reserved,
) -> Result<FeedbackStep, RunError> {
    let _ = (ctx, run, shared, slots, reserved);
    todo!("feedback::advance")
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
            let status = world.writes().join("\n");
            assert!(status.contains(&round_session().to_string()), "{status}");
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
                world.steps_of("feedback-archive"),
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
            if !world.creates().is_empty() {
                world.rig.view = round_view();
            }
            let step = world.try_go().map_err(|error| error.to_string());
            let done = matches!(step, Ok(FeedbackStep::Started { .. }));
            steps.push(step);
            if done {
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
            let mut world = healthy();
            assert!(matches!(world.go(), FeedbackStep::Decided { .. }));
            assert_eq!(world.go(), FeedbackStep::Preparing(Wait::Workspace));
            world.rig.view = round_view();
            // The session starts, and the lease is lost as its launch line is written.
            world.rig.guard = Box::new(HeldFor::new(2));
            assert!(world.try_go().is_err());
            assert_eq!(world.starts().len(), 1);
            world.rig.guard = Box::new(crate::factory::worker::AlwaysHeld(1));
            // The session record shows in the workspace with its turn: it is adopted.
            world.rig.view = session_view(SessionState::Working, Some((1, 1)));
            let step = world.go();
            assert!(
                matches!(step, FeedbackStep::Started { round: 1, .. }),
                "{step:?}"
            );
            assert_eq!(world.starts().len(), 1, "never started twice");
            assert_eq!(world.launches().len(), 2);
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
                    let steps = drive(&mut world);
                    assert!(
                        matches!(steps.last(), Some(Ok(FeedbackStep::Started { .. }))),
                        "{label}: {steps:?}"
                    );
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
            assert_eq!(world.creates().len(), 2);
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
}

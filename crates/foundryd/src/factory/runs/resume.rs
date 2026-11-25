//! Interruptions and resumes of a run.

use std::time::Duration;

use foundry_proto::factory::{RunEvent, RunId};
use foundry_proto::ids::{MachineId, ProjectId, SessionId, WorkspaceId};

use super::{RunContext, RunError};
use crate::factory::worker::FleetView;

/// How long a machine is unreachable before what it holds is given up on: 15 minutes, on the
/// sleep-counting clock that [`FleetView::unreachable_for`] reads.
pub const UNREACHABLE_FOR: Duration = Duration::from_secs(15 * 60);

/// The public reason of a run whose session's machine stayed unreachable.
pub const MACHINE_UNREACHABLE: &str = "the session's machine has been unreachable for 15 minutes";

/// The detail of the stop of a run whose pull request's branch is not a plain branch name.
pub const BRANCH_REFUSED: &str = "the pull request's branch is not a plain branch name";

/// What one pass of [`advance`] or [`placed`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResumeStep {
    /// Not this step's run, or no interruption: nothing was written.
    Skipped { reason: String },
    /// The interruption is recorded and the earlier workspace's archive asked for.
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
    /// A claim launched on another machine.
    Moved {
        machine: MachineId,
        workspace: WorkspaceId,
    },
    /// The run went stuck.
    Stuck { reason: String },
    /// A person closed the issue: the run is ended.
    Abandoned,
    /// The run's own merged pull request closed the issue: the run is ended.
    Ended,
}

/// What a pass that returned waiting is waiting for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    /// The earlier workspace still shows, and its machine is not yet unreachable for long.
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

/// One pass over `run`.
pub fn advance(_ctx: &RunContext<'_>, _run: &RunId) -> Result<ResumeStep, RunError> {
    todo!("resume::advance")
}

/// One pass over `run` with the machine placement picked.
pub fn placed(
    _ctx: &RunContext<'_>,
    _run: &RunId,
    _machine: MachineId,
) -> Result<ResumeStep, RunError> {
    todo!("resume::placed")
}

/// The workspaces of `run` that it has moved on from.
pub fn orphans(
    _view: &FleetView,
    _project: ProjectId,
    _run: &RunId,
    _events: &[RunEvent],
) -> Vec<WorkspaceId> {
    todo!("resume::orphans")
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
        Call, CreateWorkspace, FleetView, FleetViewBuilder, Lost, WorkerError,
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

    /// The session is gone: the fleet shows none.
    fn gone() -> FleetView {
        fleet(None).build()
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
        placed(&rig.ctx(), run, machine).unwrap()
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

    /// A building run whose session failed with no turn: nothing is recorded of a resume yet.
    fn interrupted() -> (Rig, RunId) {
        let mut rig = Rig::new();
        let run = run_id(1);
        building(&rig, &run);
        rig.view = failed();
        (rig, run)
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
            assert_eq!(rig.worker.calls(), vec![archived_call()]);
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

        /// The runner's case: "resumes two sessions that ended badly in one pass without any T3
        /// trouble". Foundry has no evidence to wait for: a lone session that ended is resumed
        /// at once.
        #[test]
        fn resumes_two_sessions_that_ended_badly_in_one_pass_without_any_t3_trouble() {
            let (rig, run) = interrupted();
            pass_one(&rig, &run);
            assert_eq!(interruptions(&rig, &run)[0]["resumes"], 1);
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
            // The earlier workspace still shows: the resume waits for it.
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
            assert!(
                matches!(pass(rig, run), ResumeStep::Archiving { resumes, .. } if resumes == k)
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
            let (mut rig, run) = interrupted();
            rig.view = failed();
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
            let (mut rig, run) = interrupted();
            rig.config.limits.interruption_resumes = 3;
            rig.view = failed();
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
    }

    /// The runner's `interruption.test.ts`, "interrupted: a send cut short". Foundry has no
    /// resume message to send: the steps a pass can be cut short between are the archive, the
    /// create and the start, and each is carried on from the ledger.
    mod interrupted_a_send_cut_short {
        use super::*;

        /// The runner's case: "is sent again under the same command id". Foundry: a pass cut
        /// short after the decision and before the archive records the archive and asks for it
        /// on the next pass, with no second decision; a pass after that asks for nothing more.
        #[test]
        fn is_sent_again_under_the_same_command_id() {
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
            let bytes = event_count(&rig, &run);
            assert_eq!(pass(&rig, &run), ResumeStep::Waiting(Wait::Archive));
            assert_eq!(rig.worker.calls(), vec![archived_call()]);
            assert_eq!(event_count(&rig, &run), bytes);
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
            assert_eq!(pass(&rig, &run), ResumeStep::Waiting(Wait::Archive));
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
                .filter(|e| e.event == "session_launched")
                .next_back()
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
            let message = resumed_message(&run_id(1), "t3code/abc");
            let paragraph =
                "A previous attempt of this issue (rawphp__softfac__8__1) was interrupted";
            assert!(
                message
                    .body
                    .as_deref()
                    .unwrap_or_default()
                    .contains(paragraph)
                    || message.line.contains(paragraph),
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
                .filter(|e| e.event == "session_launched")
                .next_back()
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

        /// The runner's case: "does not pass the resumes of a stuck run to a new attempt a person
        /// relabelled". Foundry: a new attempt has a ledger of its own, so its first resume is
        /// the first.
        #[test]
        fn does_not_pass_the_resumes_of_a_stuck_run_to_a_new_attempt_a_person_relabelled() {
            let mut rig = Rig::new();
            let first = run_id(1);
            building(&rig, &first);
            rig.write(
                &first,
                Event::SessionInterrupted {
                    resumes: 2,
                    relaunch: false,
                    run_ordinal: None,
                    branch: None,
                    t3_error: None,
                    ended_in_pass: 0,
                },
            );
            let second = run_id(2);
            building(&rig, &second);
            rig.view = failed();
            assert!(matches!(
                pass(&rig, &second),
                ResumeStep::Archiving { resumes: 1, .. }
            ));
            assert_eq!(interruptions(&rig, &second).len(), 1);
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
            let error = placed(&rig.ctx(), &run, m2()).unwrap_err();
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
                pr_opened(&rig, &run);
                rig.write(
                    &run,
                    Event::GateSentBack {
                        attempt: 1,
                        run_ordinal: 1,
                    },
                );
                rig.sim.set_pull(12, pr_on(head, false));
                assert!(
                    matches!(pass(&rig, &run), ResumeStep::Stuck { .. }),
                    "{head}"
                );
                assert!(rig.worker.calls().is_empty(), "{head}");
            }
        }

        /// The runner's case: "starts the next attempt from base with no branch anywhere".
        #[test]
        fn starts_the_next_attempt_from_base_with_no_branch_anywhere() {
            let mut rig = Rig::new();
            let run = run_id(1);
            building_with(&rig, &run, launch(None, Some("main")));
            rig.view = failed();
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
            assert!(rig.worker.calls().is_empty());
            // The next pass reads again and goes on.
            pass_one(&rig, &run);
        }
    }

    /// The runner's `interruption.test.ts`, "interrupted: a run with no recorded approval".
    mod interrupted_a_run_with_no_recorded_approval {
        use super::*;

        fn unapproved() -> (Rig, RunId) {
            let mut rig = Rig::new();
            let run = run_id(1);
            // The ledger of a run whose first line was lost: no `approved` step.
            rig.write(
                &run,
                Event::Claimed {
                    machine: m1().to_string(),
                    github_user: LOGIN.into(),
                },
            );
            rig.moved(&run, State::Ready, State::Triage);
            rig.step(&run, &format!("workspace-created {OLD}"));
            rig.step(&run, "approval-rechecked");
            rig.write(&run, launch(Some(BRANCH), Some("main")));
            rig.moved(&run, State::Triage, State::Building);
            rig.sim.set_labels(&["factory:building"]);
            rig.sim.set_assignees(&[LOGIN]);
            rig.view = gone();
            (rig, run)
        }

        /// The runner's case: "whose thread is gone after a T3 outage goes stuck as thread-gone,
        /// not relaunched". Foundry: a resume starts a session, which the approval must stand
        /// behind: with none recorded the run goes stuck as `start` does, naming the session's
        /// end in the ledger, and nothing is asked of a worker.
        #[test]
        fn whose_thread_is_gone_after_a_t3_outage_goes_stuck_as_thread_gone_not_relaunched() {
            let (rig, run) = unapproved();
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::Stuck {
                    reason: messages::CANNOT_CONFIRM.into()
                }
            );
            assert!(interruptions(&rig, &run).is_empty());
            assert!(create_calls(&rig).is_empty());
            assert!(archive_calls(&rig).is_empty());
            assert!(rig.finished(&run));
        }

        /// The runner's case: "found gone by its resume send goes stuck as thread-gone too".
        /// Foundry: a session found failed in a run with no approval is stuck the same way.
        #[test]
        fn found_gone_by_its_resume_send_goes_stuck_as_thread_gone_too() {
            let (mut rig, run) = unapproved();
            rig.view = failed();
            assert!(matches!(pass(&rig, &run), ResumeStep::Stuck { .. }));
            assert!(interruptions(&rig, &run).is_empty());
            assert!(
                rig.worker
                    .calls()
                    .iter()
                    .all(|call| matches!(call, Call::EndSessions { .. }))
            );
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

        /// The runner's case: "that lands between the ledger read and the thread read is not a
        /// relaunch". Foundry reads the ledger once and the fleet's snapshot once; a stop that
        /// lands after the decision is the next case.
        #[test]
        fn that_lands_between_the_ledger_read_and_the_thread_read_is_not_a_relaunch() {
            let (rig, run) = interrupted();
            stop(&rig, &run);
            assert!(!matches!(pass(&rig, &run), ResumeStep::Archiving { .. }));
            assert!(interruptions(&rig, &run).is_empty());
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
            assert_eq!(rig.worker.calls(), vec![archived_call()]);
            // Placement is not even needed while the earlier workspace shows.
            assert_eq!(pass(&rig, &run), ResumeStep::Waiting(Wait::Archive));
            assert!(create_calls(&rig).is_empty());
            // The pass that sees it archived asks for a machine, and creates nothing.
            rig.view = archived();
            assert!(matches!(pass(&rig, &run), ResumeStep::NeedsMachine(_)));
            assert!(create_calls(&rig).is_empty());
            // The pass that places asks for the workspace and never for an archive.
            rig.worker.on_create(Ok(fresh()));
            placed_on(&rig, &run, m2());
            assert_eq!(create_calls(&rig).len(), 1);
            assert_eq!(archive_calls(&rig).len(), 1);
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
                        .all(|call| matches!(call, Call::EndSessions { .. })),
                    "{head:?}"
                );
                assert!(interruptions(&rig, &run).is_empty(), "{head:?}");
                assert!(rig.finished(&run), "{head:?}");
                let stuck = rig
                    .events(&run)
                    .into_iter()
                    .find(|e| e.event == "stuck")
                    .unwrap();
                assert_eq!(stuck.cause.unwrap()["reason"], "launch-failed");
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
            let Call::Create { machine, request } = &calls[0] else {
                panic!("a create");
            };
            assert_eq!(*machine, m1());
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
                vec![Call::EndSessions { workspace: old() }]
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
            let _ = placed(&rig.ctx(), &run, m2());
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
                    .all(|call| matches!(call, Call::Create { machine, .. } if *machine == m2()))
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

        /// The session starts with the resume's first message on the pull request's branch, and
        /// the launch line and the status comment name what started.
        #[test]
        fn the_resumed_session_is_started_recorded_and_named_in_the_status() {
            let mut rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            rig.view = failed();
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
                .filter(|e| e.event == "session_launched")
                .next_back()
                .unwrap();
            assert_eq!(launched.data["thread_id"], resumed_session().to_string());
            assert_eq!(launched.data["worktree"], fresh().to_string());
            assert_eq!(launched.data["branch"], PR_BRANCH);
            assert_eq!(launched.data["base"], "main");
            assert_eq!(launched.data["resumed_from"]["branch"], PR_BRANCH);
            let status = rig.sim.comments().pop().unwrap();
            assert!(status.contains("**Factory status:** building"), "{status}");
            assert!(status.contains(&m2().to_string()), "{status}");
            assert!(status.contains(&resumed_session().to_string()), "{status}");
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
            assert_eq!(event_count(&rig, &run), 6);
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
            let Call::Create { machine, request } = &calls[0] else {
                panic!("a create");
            };
            assert_eq!(*machine, m2());
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
            assert!(placed(&rig.ctx(), &run, m1()).is_err());
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
        fn claimed_not_built(rig: &Rig, run: &RunId) {
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

        /// A session that is done and was not read is `building`'s, not an interruption; one
        /// that ended with no result line is.
        #[test]
        fn a_finished_session_with_a_done_line_is_buildings_and_one_without_is_an_interruption() {
            let (mut rig, run) = interrupted();
            rig.view = fleet(Some(SessionState::Finished))
                .turn(turn_of(session(), old(), 1, 1))
                .build();
            say(&rig, DONE_MESSAGE);
            assert!(is_skipped(&pass(&rig, &run)));
            assert!(interruptions(&rig, &run).is_empty());
            say(&rig, "I am still working on it.");
            pass_one(&rig, &run);
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
                    .all(|call| !matches!(call, Call::Turn { .. }))
            );
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
            rig.guard = Box::new(HeldFor::new(2));
            assert!(advance(&rig.ctx(), &run).is_err());
            assert!(rig.worker.calls().is_empty());
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
}

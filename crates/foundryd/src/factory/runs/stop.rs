//! How a run ends: stopped, stuck or done.

use foundry_proto::factory::{RunEvent, RunId};
use foundry_proto::ids::{MachineId, WorkspaceId};
use serde_json::Value;

use super::gating::{self, GATE_REQUESTED};
use super::progress::{Seen, read_progress};
use super::resume::UNREACHABLE_FOR;
use super::{
    Closing, Progress, RunContext, RunError, Stop, StuckStop, abandon, closed_issue, end_own_merge,
    go_stuck, progress_from, recorded_reason, workspace_id,
};
use crate::factory::gate;
use crate::factory::ledger::Ledger;
use crate::factory::ledger::events::{Cause, CauseStep, Event, names};
use crate::factory::lifecycle::State;
use crate::factory::messages::STOPPED_BY_HAND;
use crate::factory::worker::Guard;

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
    pub name: String,
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
                    name: name.clone(),
                    gate: is_gate,
                });
            }
        }
    }
    found
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
        fn refuses_a_run_that_left_building_with_a_stale_stop_line_instead_of_answering_already() {
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
}

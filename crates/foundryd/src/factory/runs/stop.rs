//! How a run ends: stopped, stuck or done.

use foundry_proto::factory::RunId;

use super::RunError;
use super::progress::{Seen, read_progress};
use crate::factory::ledger::Ledger;
use crate::factory::ledger::events::{Cause, CauseStep, Event};
use crate::factory::lifecycle::State;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::factory::ledger::events::{Cause, CauseStep, Event, Outcome};
    use crate::factory::lifecycle::State;
    use crate::factory::runs::testing::*;
    use crate::factory::worker::Lost;

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
}

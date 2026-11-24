//! Reading a run's ledger back.
//!
//! The read half of the runner's `src/ledger/read.ts`. An event is read back as it was
//! written: an event name this build does not know, or data that is not what the name says,
//! is kept as it is, so a run written by a newer build reads whole. Only a cause that is not
//! an object with a text `step` and `reason`, and the right types for the rest, reads as no
//! cause, as the runner's `readCause` has it.
//!
//! The runner's file cases (cut-off and invalid lines, blank lines, symlinks) have no
//! counterpart: each event is a record the store has already parsed. A record this build
//! cannot parse at all is an error, never skipped, since a run with an event left out reads
//! as another run.

use foundry_proto::factory::{RunEvent, RunId};
use serde_json::Value;

use super::Ledger;
use crate::store::{Store, StoreError};

impl Ledger {
    /// The events of `run`, from every writer, in order: by the lease generation each was
    /// written under, then by its writer's factory version. Never by time.
    pub fn read(&self, run: &RunId) -> Result<Vec<RunEvent>, StoreError> {
        let store = self
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        events(&store, run)
    }
}

/// The events of `run` that `store` holds, in order, as [`Ledger::read`] reads them.
pub fn events(store: &Store, run: &RunId) -> Result<Vec<RunEvent>, StoreError> {
    let mut events = store.run_events(run)?;
    for event in &mut events {
        if !is_cause(event.cause.as_ref()) {
            event.cause = None;
        }
    }
    Ok(events)
}

/// Whether `cause` reads as a cause: an object with a text `step` and `reason`, an `actor`,
/// `command` and `label` that are text when present, and an `attempt` that is a u32. Any
/// other key is kept, so a cause from a newer build reads as it was written.
fn is_cause(cause: Option<&Value>) -> bool {
    let Some(Value::Object(fields)) = cause else {
        return false;
    };
    let text = |key: &str| fields.get(key).is_some_and(Value::is_string);
    let text_or_absent =
        |key: &str| matches!(fields.get(key), None | Some(Value::Null)) || text(key);
    let attempt_ok = match fields.get("attempt") {
        None | Some(Value::Null) => true,
        Some(attempt) => attempt
            .as_u64()
            .is_some_and(|attempt| u32::try_from(attempt).is_ok()),
    };
    text("step")
        && text("reason")
        && text_or_absent("actor")
        && text_or_absent("command")
        && text_or_absent("label")
        && attempt_ok
}

/// The value at `key` of an event's `data`; `None` when the data is not an object with it.
pub fn field<'a>(data: &'a Value, key: &str) -> Option<&'a Value> {
    data.as_object()?.get(key)
}

/// The text at `key` of an event's `data`; `None` when it is not text.
pub fn text<'a>(data: &'a Value, key: &str) -> Option<&'a str> {
    field(data, key)?.as_str()
}

#[cfg(test)]
mod tests {
    use foundry_proto::factory::RunEvent;
    use foundry_proto::ids::MachineId;
    use serde_json::{Value, json};

    use super::*;
    use crate::factory::ledger::events::{Cause, CauseStep, Event};
    use crate::factory::ledger::format_ts;
    use crate::factory::ledger::testing::*;

    /// What reading `data` of `event`, written `secs` after T0 by the ledger, gives back.
    fn at(secs: u64, event: &str, data: Value) -> RunEvent {
        RunEvent {
            run: run(8),
            generation: 1,
            ts: format_ts(T0 + secs * 1000),
            ms: T0 + secs * 1000,
            event: event.into(),
            data,
            cause: Some(json!({"step": "pass", "reason": "claimed"})),
        }
    }

    /// What reading an event another writer left at T0 with `cause` gives back.
    fn as_left(event: &str, data: Value, cause: Option<Value>) -> RunEvent {
        RunEvent {
            run: run(8),
            generation: 1,
            ts: format_ts(T0),
            ms: T0,
            event: event.into(),
            data,
            cause,
        }
    }

    #[test]
    fn read_gives_back_every_event_type_equal() {
        let (_dir, store) = open_store();
        let ledger = ledger(&store, MachineId::random(), 1, ticking(T0, 1000));
        let samples: Vec<(Event, String)> = samples()
            .into_iter()
            .filter(|(event, _)| !matches!(event, Event::Finished { .. }))
            .collect();
        for (event, _) in &samples {
            ledger.write(&run(8), event, &test_cause()).unwrap();
        }
        let read = ledger.read(&run(8)).unwrap();
        let expected: Vec<RunEvent> = (0..)
            .zip(&samples)
            .map(|(i, (event, data))| {
                at(
                    i,
                    event.parts().0,
                    serde_json::from_str::<Value>(data).unwrap(),
                )
            })
            .collect();
        assert_eq!(read, expected);
        let typed: Vec<Option<Event>> = read
            .iter()
            .map(|event| Event::from_parts(&event.event, &event.data))
            .collect();
        let given: Vec<Option<Event>> = samples.into_iter().map(|(event, _)| Some(event)).collect();
        assert_eq!(typed, given);
    }

    #[test]
    fn read_an_empty_file_is_an_empty_untruncated_record() {
        // A run with no events reads as none.
        let (_dir, store) = open_store();
        let ledger = ledger(&store, MachineId::random(), 1, ticking(T0, 1000));
        assert_eq!(ledger.read(&run(8)).unwrap(), []);
        assert_eq!(events(&store.lock().unwrap(), &run(8)).unwrap(), []);
    }

    #[test]
    fn read_keeps_an_unknown_event_type_as_written() {
        let (_dir, store) = open_store();
        put(
            &store,
            MachineId::random(),
            1,
            &run(8),
            1,
            T0,
            "merged",
            json!({"sha": "abc", "n": 1}),
            None,
        );
        assert_eq!(
            events(&store.lock().unwrap(), &run(8)).unwrap(),
            [as_left("merged", json!({"sha": "abc", "n": 1}), None)]
        );
    }

    #[test]
    fn read_keeps_a_known_type_with_the_wrong_data_and_a_missing_data_key_as_written() {
        let (_dir, store) = open_store();
        let other = MachineId::random();
        put(
            &store,
            other,
            1,
            &run(8),
            1,
            T0,
            "claimed",
            json!({"machine": 1}),
            None,
        );
        put(&store, other, 2, &run(8), 1, T0, "stuck", Value::Null, None);
        assert_eq!(
            events(&store.lock().unwrap(), &run(8)).unwrap(),
            [
                as_left("claimed", json!({"machine": 1}), None),
                as_left("stuck", Value::Null, None),
            ]
        );
    }

    #[test]
    fn read_keeps_a_state_changed_with_an_unknown_state_name_as_written() {
        let (_dir, store) = open_store();
        put(
            &store,
            MachineId::random(),
            1,
            &run(8),
            1,
            T0,
            "state_changed",
            json!({"from": "building", "to": "merged"}),
            None,
        );
        let read = events(&store.lock().unwrap(), &run(8)).unwrap();
        assert_eq!(
            read,
            [as_left(
                "state_changed",
                json!({"from": "building", "to": "merged"}),
                None
            )]
        );
        assert_eq!(Event::from_parts(&read[0].event, &read[0].data), None);
    }

    #[test]
    fn lines_from_before_the_pr_end_and_model_fields_read_as_written() {
        let (_dir, store) = open_store();
        let other = MachineId::random();
        let launched = json!({"thread_id": "thr_1", "worktree": null, "branch": null});
        let finished = json!({
            "outcome": "pr-open",
            "total_ms": 3000,
            "gate_attempts": 1,
            "review_cycles": null,
            "pr_url": null,
        });
        let cause = Some(json!({"step": "pass", "reason": "claimed"}));
        put(
            &store,
            other,
            1,
            &run(8),
            1,
            T0,
            "session_launched",
            launched.clone(),
            cause.clone(),
        );
        put(
            &store,
            other,
            2,
            &run(8),
            1,
            T0,
            "finished",
            finished.clone(),
            cause.clone(),
        );
        let read = events(&store.lock().unwrap(), &run(8)).unwrap();
        assert_eq!(read[0].data, launched);
        assert_eq!(read[1].data, finished);
    }

    #[test]
    fn a_stuck_line_with_a_branch_and_commits_round_trips_and_an_old_one_reads_without() {
        let (_dir, store) = open_store();
        let ledger = ledger(&store, MachineId::random(), 1, ticking(T0, 1));
        let event = Event::Stuck {
            reason: "the run passed its time limit".into(),
            branch: Some("t3code/abc".into()),
            commits_ahead: Some(2),
        };
        ledger.write(&run(8), &event, &test_cause()).unwrap();
        let read = ledger.read(&run(8)).unwrap();
        assert_eq!(
            read[0].data,
            json!({"reason": "the run passed its time limit", "branch": "t3code/abc", "commits_ahead": 2})
        );
        assert_eq!(
            Event::from_parts(&read[0].event, &read[0].data),
            Some(event)
        );

        let old = json!({"reason": "gate failed 3 times"});
        assert_eq!(
            Event::from_parts("stuck", &old),
            Some(Event::Stuck {
                reason: "gate failed 3 times".into(),
                branch: None,
                commits_ahead: None,
            })
        );
    }

    #[test]
    fn a_cause_attempt_that_is_not_a_u32_is_left_out_so_the_cause_still_reads_back() {
        let (_dir, store) = open_store();
        let ledger = ledger(&store, MachineId::random(), 1, ticking(T0, 0));
        ledger
            .write(
                &run(8),
                &step("s"),
                &Cause::new(CauseStep::Gate, "gate-regated").attempt(u32::MAX),
            )
            .unwrap();
        assert_eq!(
            ledger.read(&run(8)).unwrap()[0].cause,
            Some(json!({"step": "gate", "reason": "gate-regated", "attempt": 4_294_967_295_u64}))
        );
    }

    #[test]
    fn a_cause_that_is_not_an_object_with_a_step_and_a_reason_reads_as_no_cause_and_one_that_is_keeps_its_extra_keys()
     {
        let causes = [
            (Some(Value::Null), None),
            (Some(json!("claimed")), None),
            (Some(json!(["pass", "claimed"])), None),
            (Some(json!({"step": "pass"})), None),
            (Some(json!({"reason": "claimed"})), None),
            (Some(json!({"step": 1, "reason": "claimed"})), None),
            (
                Some(json!({"step": "pass", "reason": "claimed", "attempt": "1"})),
                None,
            ),
            (
                Some(json!({"step": "pass", "reason": "claimed", "attempt": -1})),
                None,
            ),
            (
                Some(json!({"step": "pass", "reason": "claimed", "attempt": 4_294_967_296_u64})),
                None,
            ),
            (
                Some(json!({"step": "pass", "reason": "claimed", "actor": 5})),
                None,
            ),
            (
                Some(json!({
                    "step": "newer",
                    "reason": "unknown-code",
                    "actor": "alice",
                    "attempt": 2,
                    "extra": {"k": 1},
                })),
                Some(json!({
                    "step": "newer",
                    "reason": "unknown-code",
                    "actor": "alice",
                    "attempt": 2,
                    "extra": {"k": 1},
                })),
            ),
            (None, None),
        ];
        let (_dir, store) = open_store();
        let other = MachineId::random();
        for (version, (cause, _)) in (1..).zip(&causes) {
            put(
                &store,
                other,
                version,
                &run(8),
                1,
                T0,
                "stuck",
                json!({"reason": "r"}),
                cause.clone(),
            );
        }
        let read: Vec<Option<Value>> = events(&store.lock().unwrap(), &run(8))
            .unwrap()
            .into_iter()
            .map(|event| event.cause)
            .collect();
        let expected: Vec<Option<Value>> = causes.into_iter().map(|(_, read)| read).collect();
        assert_eq!(read, expected);
    }

    #[test]
    fn a_field_of_the_data_is_read_by_its_key_and_text_only_when_it_is_text() {
        let data = json!({"step": "red", "pr": 12});
        assert_eq!(field(&data, "pr"), Some(&json!(12)));
        assert_eq!(field(&data, "url"), None);
        assert_eq!(field(&Value::Null, "pr"), None);
        assert_eq!(text(&data, "step"), Some("red"));
        assert_eq!(text(&data, "pr"), None);
        assert_eq!(text(&json!(["step"]), "step"), None);
    }
}

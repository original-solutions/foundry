//! The events a run's ledger holds.

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::factory::ledger::testing::samples;

    /// The keys of `data` in the order `text` writes them.
    fn keys_in_order(text: &str) -> Vec<String> {
        // The data object is the last value of `{"event":..,"data":{..}}`, and every sample's
        // values are flat or nested one level, so its keys are those at depth 2.
        let mut keys = Vec::new();
        let mut depth = 0;
        let mut chars = text.char_indices().peekable();
        let mut in_string = false;
        let mut start = 0;
        let mut after_brace_or_comma = false;
        while let Some((i, c)) = chars.next() {
            if in_string {
                match c {
                    '\\' => {
                        chars.next();
                    }
                    '"' => {
                        in_string = false;
                        if depth == 2 && after_brace_or_comma {
                            keys.push(text[start..i].to_owned());
                        }
                        after_brace_or_comma = false;
                    }
                    _ => {}
                }
                continue;
            }
            match c {
                '"' => {
                    in_string = true;
                    start = i + 1;
                }
                '{' | '[' => {
                    depth += 1;
                    after_brace_or_comma = c == '{';
                }
                '}' | ']' => depth -= 1,
                ',' => after_brace_or_comma = true,
                _ => {}
            }
        }
        keys
    }

    #[test]
    fn event_name_is_the_snake_case_type_and_data_holds_exactly_the_field_list_in_order() {
        for (event, data) in samples() {
            let text = serde_json::to_string(&event).unwrap();
            let (name, written) = event.parts();
            assert_eq!(text, format!(r#"{{"event":"{name}","data":{data}}}"#));
            assert_eq!(written, serde_json::from_str::<Value>(&data).unwrap(), "{name}");
            let fields = fields(name).unwrap();
            // A field that is left out is not written; every other one is, in order.
            let expected: Vec<String> = fields
                .iter()
                .filter(|field| written.get(**field).is_some())
                .map(|field| (*field).to_owned())
                .collect();
            assert_eq!(keys_in_order(&text), expected, "{name}");
            assert_eq!(written.as_object().unwrap().len(), expected.len(), "{name}");
        }
    }

    #[test]
    fn append_writes_the_fields_in_the_doc_order_whatever_order_the_caller_built_the_object_in()
    {
        // The runner reorders a hand-built object; here the type has one order, and the nested
        // shapes keep the runner's order too.
        let event: Event = serde_json::from_value(json!({
            "event": "session_launched",
            "data": {
                "resumed_from": {"branch": "t3code/abc", "run": "r"},
                "model_options": null,
                "model": "m",
                "model_instance": "i",
                "branch": "b",
                "worktree": "w",
                "thread_id": "t",
            },
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"event":"session_launched","data":{"thread_id":"t","worktree":"w","branch":"b","model_instance":"i","model":"m","model_options":null,"resumed_from":{"run":"r","branch":"t3code/abc"}}}"#
        );
        let checks: Event = serde_json::from_value(json!({
            "event": "checks_read",
            "data": {"checks": [{"state": "passed", "name": "gate"}], "sha": "abc"},
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_string(&checks).unwrap(),
            r#"{"event":"checks_read","data":{"sha":"abc","checks":[{"name":"gate","state":"passed"}]}}"#
        );
        assert_eq!(RESUMED_FROM_FIELDS, ["run", "branch"]);
        assert_eq!(CHECK_FIELDS, ["name", "state"]);
    }

    #[test]
    fn every_event_reads_back_from_its_name_and_data_as_itself() {
        for (event, _) in samples() {
            let (name, data) = event.parts();
            assert_eq!(Event::from_parts(name, &data), Some(event), "{name}");
        }
        assert_eq!(Event::from_parts("merged", &json!({"sha": "abc"})), None);
        assert_eq!(Event::from_parts("claimed", &json!({"machine": 1})), None);
    }

    #[test]
    fn the_field_table_names_the_33_events_once_each() {
        assert_eq!(FIELDS.len(), 33);
        let mut names: Vec<&str> = FIELDS.iter().map(|(name, _)| *name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 33);
        assert_eq!(fields("finished").unwrap(), FIELDS[32].1);
        assert_eq!(fields("merged"), None);
        assert_eq!(
            CAUSE_FIELDS,
            ["step", "reason", "actor", "command", "attempt", "label"]
        );
    }

    #[test]
    fn a_cause_cleans_actor_and_label_of_control_and_bidi_characters_and_cuts_them_at_100_and_cuts_a_command_at_1000()
     {
        let actor = Cause::new(CauseStep::Start, "approved")
            .actor("al\u{0007}i\u{202e}ce\u{2066}\u{0000}\n\u{007f}\u{0085}");
        assert_eq!(actor.actor.as_deref(), Some("alice"));
        let label = Cause::new(CauseStep::Start, "approved").label(&"x".repeat(150));
        assert_eq!(label.label, Some("x".repeat(100)));
        // Characters, not bytes.
        let emoji = Cause::new(CauseStep::Start, "approved").actor(&"\u{1f600}".repeat(150));
        assert_eq!(emoji.actor.unwrap().chars().count(), 100);
        let command = Cause::new(CauseStep::Gate, "gate-command-ran").command(&"c".repeat(1500));
        assert_eq!(command.command, Some("c".repeat(1000)));
        // A command is cleaned the same way as the others: one line, no control characters.
        assert_eq!(
            Cause::new(CauseStep::Gate, "gate-command-ran")
                .command("a\nb\tc")
                .command
                .as_deref(),
            Some("abc")
        );
        // Nothing else goes in a cause, and an unset extra is left out.
        assert_eq!(
            serde_json::to_string(&Cause::new(CauseStep::Pass, "claimed")).unwrap(),
            r#"{"step":"pass","reason":"claimed"}"#
        );
    }

    #[test]
    fn a_cause_reason_is_cleaned_and_cut_at_100_on_the_way_to_the_file() {
        let cause = Cause {
            step: CauseStep::Pass,
            reason: format!("a\u{202e}\n{}", "r".repeat(150)),
            actor: None,
            command: None,
            attempt: Some(u32::MAX),
            label: None,
        };
        assert_eq!(
            serde_json::to_value(cause.cleaned()).unwrap(),
            json!({"step": "pass", "reason": format!("a{}", "r".repeat(99)), "attempt": 4_294_967_295_u64})
        );
    }

    #[test]
    fn steps_and_kinds_are_written_as_the_runner_writes_them() {
        let steps = [
            (CauseStep::Start, "start"),
            (CauseStep::Watch, "watch"),
            (CauseStep::Forward, "forward"),
            (CauseStep::Advance, "advance"),
            (CauseStep::Gate, "gate"),
            (CauseStep::Pass, "pass"),
            (CauseStep::Retro, "retro"),
            (CauseStep::Operator, "operator"),
        ];
        for (step, name) in steps {
            assert_eq!(serde_json::to_value(step).unwrap(), json!(name));
        }
        let kinds = [
            (ErrorKind::Github, "github"),
            (ErrorKind::Tracker, "tracker"),
            (ErrorKind::T3, "t3"),
            (ErrorKind::Git, "git"),
            (ErrorKind::Gate, "gate"),
            (ErrorKind::Ledger, "ledger"),
            (ErrorKind::Config, "config"),
        ];
        for (kind, name) in kinds {
            assert_eq!(serde_json::to_value(kind).unwrap(), json!(name));
        }
        assert_eq!(
            serde_json::to_value(OptionValue::Flag(true)).unwrap(),
            json!(true)
        );
    }
}

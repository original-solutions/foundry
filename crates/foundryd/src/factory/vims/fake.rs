#[cfg(test)]
mod tests {
    use super::*;
    use crate::factory::vims::{Egress, Exec, Limit, OpState, Vims, VimsError};

    const NEW_OP: &str = r#"{"id":"op_1","status":"running","workspaceId":"wsp_1"}"#;

    fn exec(argv: &[&str], stdin: &[u8]) -> Exec {
        Exec {
            cwd: Some("/workspace/repo".into()),
            timeout_seconds: Some(1800),
            no_wait: true,
            argv: argv.iter().map(|part| part.to_string()).collect(),
            stdin: stdin.to_vec(),
        }
    }

    #[test]
    fn a_call_is_answered_by_the_script_for_its_command() {
        let fake = FakeVims::new();
        fake.on("operation show", Reply::ok(r#"{"id":"op_9","status":"succeeded","result":{"exitCode":0}}"#))
            .on("new", Reply::ok(NEW_OP));
        let shown = fake.operation("op_9").unwrap();
        assert_eq!(shown.id, "op_9");
        assert_eq!(shown.exit_code, Some(0));
        let made = fake.new_workspace("golden:abc", Egress::Open, "k1").unwrap();
        assert_eq!(made.workspace.as_deref(), Some("wsp_1"));
    }

    #[test]
    fn answers_for_one_command_come_in_the_order_scripted() {
        let fake = FakeVims::new();
        fake.on("operation show", Reply::ok(r#"{"id":"op_1","status":"running"}"#))
            .on("operation show", Reply::ok(r#"{"id":"op_1","status":"succeeded"}"#));
        assert_eq!(fake.operation("op_1").unwrap().state, OpState::Running);
        assert_eq!(fake.operation("op_1").unwrap().state, OpState::Finished);
    }

    #[test]
    fn a_call_nothing_answers_could_not_run() {
        let fake = FakeVims::new();
        let error = fake.operation("op_1").unwrap_err();
        assert!(matches!(error, VimsError::NotRun { .. }), "{error:?}");
        assert!(error.to_string().contains("no answer scripted"), "{error}");
    }

    #[test]
    fn a_repeated_key_is_answered_with_the_first_operation_and_spends_no_script() {
        let fake = FakeVims::new();
        fake.on("new", Reply::ok(NEW_OP)).on(
            "new",
            Reply::ok(r#"{"id":"op_2","status":"running","workspaceId":"wsp_2"}"#),
        );
        let first = fake.new_workspace("golden:abc", Egress::Open, "k1").unwrap();
        let again = fake.new_workspace("golden:abc", Egress::Open, "k1").unwrap();
        assert_eq!(again, first);
        let next = fake.new_workspace("golden:abc", Egress::Open, "k2").unwrap();
        assert_eq!(next.id, "op_2");
        assert_eq!(fake.calls().len(), 3);
    }

    #[test]
    fn a_reused_key_with_a_different_request_is_refused() {
        let fake = FakeVims::new();
        fake.on("new", Reply::ok(NEW_OP));
        fake.new_workspace("golden:abc", Egress::Open, "k1").unwrap();
        let error = fake
            .new_workspace("golden:abc", Egress::Closed, "k1")
            .unwrap_err();
        let refusal = error.refusal().expect("a refusal");
        assert_eq!(refusal.code, "IDEMPOTENCY_KEY_REUSED");
        assert_eq!(refusal.limit, None);
        // The first request still holds the key.
        assert_eq!(
            fake.new_workspace("golden:abc", Egress::Open, "k1").unwrap().id,
            "op_1"
        );
    }

    #[test]
    fn standard_input_is_part_of_the_request_a_key_holds() {
        let fake = FakeVims::new();
        fake.on("exec", Reply::ok(r#"{"id":"op_5","status":"running"}"#));
        fake.exec("wsp_1", &exec(&["cat"], b"one"), "k1").unwrap();
        assert_eq!(
            fake.exec("wsp_1", &exec(&["cat"], b"one"), "k1").unwrap().id,
            "op_5"
        );
        let error = fake.exec("wsp_1", &exec(&["cat"], b"two"), "k1").unwrap_err();
        assert_eq!(error.code(), Some("IDEMPOTENCY_KEY_REUSED"));
    }

    #[test]
    fn a_dropped_answer_is_no_answer_but_the_call_was_taken() {
        let fake = FakeVims::new();
        fake.on("new", Reply::ok(NEW_OP).dropped());
        let error = fake
            .new_workspace("golden:abc", Egress::Open, "k1")
            .unwrap_err();
        assert!(matches!(error, VimsError::Timeout { seconds: 30, .. }), "{error:?}");
        let found = fake.operations_by_key("k1").unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "op_1");
        assert_eq!(found[0].workspace.as_deref(), Some("wsp_1"));
        // Sent again with the same key, the call is answered with what Vims took.
        assert_eq!(
            fake.new_workspace("golden:abc", Egress::Open, "k1").unwrap().id,
            "op_1"
        );
    }

    #[test]
    fn a_key_never_sent_finds_no_operation() {
        let fake = FakeVims::new();
        fake.on("new", Reply::ok(NEW_OP));
        fake.new_workspace("golden:abc", Egress::Open, "k1").unwrap();
        assert_eq!(fake.operations_by_key("k9").unwrap(), vec![]);
    }

    #[test]
    fn a_call_that_never_reached_vims_holds_no_key() {
        let fake = FakeVims::new();
        fake.on("new", Reply::timeout()).on("new", Reply::ok(NEW_OP));
        assert!(matches!(
            fake.new_workspace("golden:abc", Egress::Open, "k1"),
            Err(VimsError::Timeout { .. })
        ));
        assert_eq!(fake.operations_by_key("k1").unwrap(), vec![]);
        assert_eq!(
            fake.new_workspace("golden:abc", Egress::Open, "k1").unwrap().id,
            "op_1"
        );
    }

    #[test]
    fn a_refusal_is_kept_by_its_key_and_the_next_attempt_is_a_new_call() {
        let fake = FakeVims::new();
        fake.on("new", Reply::refused("QUOTA_EXCEEDED", "no room for 4 vCPU"))
            .on("new", Reply::ok(NEW_OP));
        let refused = fake
            .new_workspace("golden:abc", Egress::Open, "k1")
            .unwrap_err();
        assert_eq!(refused.refusal().unwrap().limit, Some(Limit::Quota));
        assert_eq!(
            fake.new_workspace("golden:abc", Egress::Open, "k1")
                .unwrap_err(),
            refused
        );
        assert_eq!(
            fake.new_workspace("golden:abc", Egress::Open, "k2").unwrap().id,
            "op_1"
        );
    }

    #[test]
    fn a_scripted_operation_list_answers_before_the_kept_operations() {
        let fake = FakeVims::new();
        fake.on("operation list", Reply::ok("[]")).on("new", Reply::ok(NEW_OP).dropped());
        let _ = fake.new_workspace("golden:abc", Egress::Open, "k1");
        assert_eq!(fake.operations_by_key("k1").unwrap(), vec![]);
        assert_eq!(fake.operations_by_key("k1").unwrap().len(), 1);
    }

    #[test]
    fn every_call_is_recorded_in_order_with_its_input() {
        let fake = FakeVims::new();
        fake.on("exec", Reply::ok(r#"{"id":"op_5","status":"running"}"#));
        fake.exec("wsp_1", &exec(&["cat"], b"body"), "k1").unwrap();
        let _ = fake.show("wsp_1");
        let calls = fake.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].command(), "exec");
        assert_eq!(calls[0].stdin(), b"body");
        assert_eq!(calls[0].key(), Some("k1"));
        assert_eq!(calls[1].command(), "show");
        assert_eq!(calls[1].argv(), ["show", "wsp_1", "--json"]);
    }

    #[test]
    fn a_fake_is_vims_other_tasks_can_share() {
        let fake = std::sync::Arc::new(FakeVims::new());
        let vims: std::sync::Arc<dyn Vims> = fake.clone();
        fake.on("show", Reply::ok(r#"{"id":"wsp_1","status":"running"}"#));
        assert_eq!(vims.show("wsp_1").unwrap().id, "wsp_1");
        assert_eq!(fake.calls().len(), 1);
    }
}

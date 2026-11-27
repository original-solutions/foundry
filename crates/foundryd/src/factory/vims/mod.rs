pub mod cli;
pub mod fake;
pub mod keys;

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(fake: &FakeVims) -> Vec<String> {
        fake.calls().last().expect("a call").argv()
    }

    fn strings(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| part.to_string()).collect()
    }

    #[test]
    fn new_makes_a_workspace_from_a_source_key_with_egress_closed_only_when_asked() {
        let fake = FakeVims::new();
        fake.on("new", Reply::ok(r#"{"id":"op_1"}"#))
            .on("new", Reply::ok(r#"{"id":"op_2"}"#));
        fake.new_workspace("golden:abc", Egress::Open, "k1").unwrap();
        assert_eq!(
            argv(&fake),
            strings(&["new", "golden:abc", "--idempotency-key", "k1", "--json"])
        );
        fake.new_workspace("golden:abc", Egress::Closed, "k2").unwrap();
        assert_eq!(
            argv(&fake),
            strings(&[
                "new",
                "golden:abc",
                "--egress",
                "closed",
                "--idempotency-key",
                "k2",
                "--json"
            ])
        );
    }

    #[test]
    fn exec_runs_an_argv_after_dashes_with_its_input_on_stdin_and_the_cwd_without_its_slash() {
        let fake = FakeVims::new();
        fake.on("exec", Reply::ok(r#"{"id":"op_1"}"#))
            .on("exec", Reply::ok(r#"{"id":"op_2"}"#));
        let exec = Exec {
            cwd: Some("/workspace/tc-supplier-portal-v2".into()),
            timeout_seconds: Some(1800),
            no_wait: true,
            argv: strings(&["bash", "-lc", "composer install --no-interaction"]),
            stdin: b"the body".to_vec(),
        };
        fake.exec("wsp_1", &exec, "k1").unwrap();
        assert_eq!(
            argv(&fake),
            strings(&[
                "exec",
                "wsp_1",
                "--no-wait",
                "--cwd",
                "workspace/tc-supplier-portal-v2",
                "--timeout",
                "1800",
                "--idempotency-key",
                "k1",
                "--json",
                "--",
                "bash",
                "-lc",
                "composer install --no-interaction",
            ])
        );
        assert_eq!(fake.calls()[0].stdin(), b"the body");

        let plain = Exec {
            cwd: None,
            timeout_seconds: None,
            no_wait: false,
            argv: strings(&["true"]),
            stdin: Vec::new(),
        };
        fake.exec("wsp_1", &plain, "k2").unwrap();
        assert_eq!(
            argv(&fake),
            strings(&[
                "exec",
                "wsp_1",
                "--idempotency-key",
                "k2",
                "--json",
                "--",
                "true"
            ])
        );
    }

    #[test]
    fn each_operation_command_has_its_shape() {
        let fake = FakeVims::new();
        fake.on("operation show", Reply::ok(r#"{"id":"op_1"}"#))
            .on("operation list", Reply::ok("[]"))
            .on("operation logs", Reply::ok("line one\n"))
            .on("operation cancel", Reply::ok(r#"{"id":"op_1"}"#));
        fake.operation("op_1").unwrap();
        assert_eq!(argv(&fake), strings(&["operation", "show", "op_1", "--json"]));
        fake.operations_by_key("k1").unwrap();
        assert_eq!(
            argv(&fake),
            strings(&["operation", "list", "--idempotency-key", "k1", "--json"])
        );
        assert_eq!(fake.calls().last().unwrap().key(), None);
        fake.operation_logs("op_1").unwrap();
        assert_eq!(argv(&fake), strings(&["operation", "logs", "op_1", "--json"]));
        fake.cancel("op_1", "k2").unwrap();
        assert_eq!(
            argv(&fake),
            strings(&[
                "operation",
                "cancel",
                "op_1",
                "--idempotency-key",
                "k2",
                "--json"
            ])
        );
    }

    #[test]
    fn each_workspace_change_carries_its_key() {
        let fake = FakeVims::new();
        for command in ["pause", "start", "stop", "remove"] {
            fake.on(command, Reply::ok("{}"));
        }
        fake.pause("wsp_1", "k1").unwrap();
        assert_eq!(
            argv(&fake),
            strings(&["pause", "wsp_1", "--idempotency-key", "k1", "--json"])
        );
        fake.start("wsp_1", "k2").unwrap();
        assert_eq!(
            argv(&fake),
            strings(&["start", "wsp_1", "--idempotency-key", "k2", "--json"])
        );
        fake.stop("wsp_1", "k3").unwrap();
        assert_eq!(
            argv(&fake),
            strings(&["stop", "wsp_1", "--idempotency-key", "k3", "--json"])
        );
        fake.remove("wsp_1", "k4").unwrap();
        assert_eq!(
            argv(&fake),
            strings(&[
                "remove",
                "wsp_1",
                "--in-use",
                "--idempotency-key",
                "k4",
                "--json"
            ])
        );
    }

    #[test]
    fn hold_is_put_on_and_let_go_with_no_key() {
        let fake = FakeVims::new();
        fake.on("hold", Reply::ok("{}")).on("hold", Reply::ok("{}"));
        fake.hold("wsp_1", "foundry", true).unwrap();
        assert_eq!(argv(&fake), strings(&["hold", "wsp_1", "foundry", "--json"]));
        fake.hold("wsp_1", "foundry", false).unwrap();
        assert_eq!(
            argv(&fake),
            strings(&["hold", "wsp_1", "foundry", "off", "--json"])
        );
        assert!(fake.calls().iter().all(|call| call.key().is_none()));
    }

    #[test]
    fn a_grant_is_read_only_and_put_and_removed_with_its_key() {
        let fake = FakeVims::new();
        fake.on("workspace turn-grant put", Reply::ok("{}"))
            .on("workspace turn-grant rm", Reply::ok("{}"));
        fake.grant("wsp_1", "fetch", "conn_gh", "30m", "k1").unwrap();
        assert_eq!(
            argv(&fake),
            strings(&[
                "workspace",
                "turn-grant",
                "put",
                "wsp_1",
                "--turn",
                "fetch",
                "--read-only",
                "conn_gh",
                "--for",
                "30m",
                "--idempotency-key",
                "k1",
                "--json"
            ])
        );
        fake.ungrant("wsp_1", "fetch", "conn_gh", "k2").unwrap();
        assert_eq!(
            argv(&fake),
            strings(&[
                "workspace",
                "turn-grant",
                "rm",
                "wsp_1",
                "--turn",
                "fetch",
                "conn_gh",
                "--idempotency-key",
                "k2",
                "--json"
            ])
        );
    }

    #[test]
    fn each_read_has_its_shape() {
        let fake = FakeVims::new();
        fake.on("show", Reply::ok(r#"{"id":"wsp_1"}"#))
            .on("templates show", Reply::ok(r#"{"name":"t"}"#))
            .on("connection list", Reply::ok("[]"))
            .on("github", Reply::ok("{}"))
            .on("profile show", Reply::ok("{}"));
        fake.show("wsp_1").unwrap();
        assert_eq!(argv(&fake), strings(&["show", "wsp_1", "--json"]));
        fake.template("tc-supplier-portal-v2").unwrap();
        assert_eq!(
            argv(&fake),
            strings(&["templates", "show", "tc-supplier-portal-v2", "--json"])
        );
        fake.connections().unwrap();
        assert_eq!(argv(&fake), strings(&["connection", "list", "--json"]));
        fake.github().unwrap();
        assert_eq!(argv(&fake), strings(&["github", "--json"]));
        fake.profile_has_skill("factory-build").unwrap();
        assert_eq!(argv(&fake), strings(&["profile", "show", "--json"]));
        assert!(fake.calls().iter().all(|call| call.key().is_none()));
    }

    #[test]
    fn an_exec_operation_gives_its_exit_code() {
        let fake = FakeVims::new();
        fake.on(
            "operation show",
            Reply::ok(r#"{"id":"op_3","status":"succeeded","result":{"exitCode":2,"output":"tail"}}"#),
        );
        let operation = fake.operation("op_3").unwrap();
        assert_eq!(operation.state, OpState::Finished);
        assert_eq!(operation.exit_code, Some(2));
        assert_eq!(operation.error, None);
    }

    #[test]
    fn a_command_past_its_limit_is_a_failed_operation_with_host_command_timeout() {
        let fake = FakeVims::new();
        fake.on(
            "operation show",
            Reply::ok(r#"{"id":"op_3","status":"failed","error":{"code":"HOST_COMMAND_TIMEOUT","message":"timed out"}}"#),
        );
        let operation = fake.operation("op_3").unwrap();
        assert_eq!(operation.state, OpState::Failed);
        assert_eq!(operation.exit_code, None);
        assert_eq!(operation.error.as_deref(), Some("HOST_COMMAND_TIMEOUT"));
    }

    #[test]
    fn each_operation_state_is_read() {
        for (status, state) in [
            ("queued", OpState::Running),
            ("pending", OpState::Running),
            ("running", OpState::Running),
            ("succeeded", OpState::Finished),
            ("completed", OpState::Finished),
            ("failed", OpState::Failed),
            ("cancelled", OpState::Cancelled),
            ("canceled", OpState::Cancelled),
            ("Weird", OpState::Other("weird".into())),
        ] {
            let fake = FakeVims::new();
            fake.on(
                "operation show",
                Reply::ok(format!(r#"{{"id":"op_1","status":"{status}"}}"#)),
            );
            assert_eq!(fake.operation("op_1").unwrap().state, state, "{status}");
        }
    }

    #[test]
    fn an_operation_list_is_read_as_an_array_or_under_operations() {
        let fake = FakeVims::new();
        fake.on("operation list", Reply::ok(r#"[{"id":"op_1"}]"#))
            .on("operation list", Reply::ok(r#"{"operations":[{"id":"op_2"}]}"#))
            .on("operation list", Reply::ok(r#"{"operations":[]}"#));
        assert_eq!(fake.operations_by_key("k").unwrap()[0].id, "op_1");
        assert_eq!(fake.operations_by_key("k").unwrap()[0].id, "op_2");
        assert!(fake.operations_by_key("k").unwrap().is_empty());
    }

    #[test]
    fn operation_logs_are_read_as_text_or_from_json() {
        let fake = FakeVims::new();
        fake.on("operation logs", Reply::ok(r#"{"logs":"one\ntwo\n"}"#))
            .on("operation logs", Reply::ok("plain\n"));
        assert_eq!(fake.operation_logs("op_1").unwrap(), "one\ntwo\n");
        assert_eq!(fake.operation_logs("op_1").unwrap(), "plain\n");
    }

    #[test]
    fn a_change_with_an_operation_gives_it_and_one_without_is_done() {
        let fake = FakeVims::new();
        fake.on("pause", Reply::ok(r#"{"operation":{"id":"op_4","status":"running"}}"#))
            .on("pause", Reply::ok(r#"{"workspace":{"id":"wsp_1","status":"paused"}}"#))
            .on("pause", Reply::ok(""));
        assert!(matches!(
            fake.pause("wsp_1", "k1").unwrap(),
            Change::Operation(operation) if operation.id == "op_4"
        ));
        assert_eq!(fake.pause("wsp_1", "k2").unwrap(), Change::Done);
        assert_eq!(fake.pause("wsp_1", "k3").unwrap(), Change::Done);
    }

    #[test]
    fn show_gives_the_workspace_s_state_and_its_source_key() {
        let fake = FakeVims::new();
        fake.on(
            "show",
            Reply::ok(r#"{"workspace":{"id":"wsp_1","name":"brave-otter","status":"Paused","versions":{"base":{"variantDigest":"sha256:e355a3c80fa7"}}}}"#),
        );
        let workspace = fake.show("wsp_1").unwrap();
        assert_eq!(workspace.id, "wsp_1");
        assert_eq!(workspace.name.as_deref(), Some("brave-otter"));
        assert_eq!(workspace.state, WorkspaceState::Paused);
        assert_eq!(workspace.source_key.as_deref(), Some("golden:e355a3c80fa7"));
    }

    #[test]
    fn each_workspace_state_is_read() {
        for (status, state) in [
            ("running", WorkspaceState::Running),
            ("Paused", WorkspaceState::Paused),
            ("stopped", WorkspaceState::Stopped),
            ("sleeping", WorkspaceState::Asleep),
            ("asleep", WorkspaceState::Asleep),
            ("starting", WorkspaceState::Other("starting".into())),
        ] {
            let fake = FakeVims::new();
            fake.on(
                "show",
                Reply::ok(format!(r#"{{"id":"wsp_1","status":"{status}"}}"#)),
            );
            assert_eq!(fake.show("wsp_1").unwrap().state, state, "{status}");
        }
    }

    #[test]
    fn a_template_gives_its_source_key_checkout_path_size_and_whether_it_is_retired() {
        let fake = FakeVims::new();
        fake.on(
            "templates show",
            Reply::ok(r#"{"template":{"name":"tc-supplier-portal-v2","sourceKey":"golden:abc123","vcpu":4,"retired":false,"sources":[{"repo":"Trilogy-Care/tc-supplier-portal-v2","path":"/workspace/tc-supplier-portal-v2"}]}}"#),
        );
        let template = fake.template("tc-supplier-portal-v2").unwrap();
        assert_eq!(template.name, "tc-supplier-portal-v2");
        assert_eq!(template.source_key.as_deref(), Some("golden:abc123"));
        assert_eq!(template.vcpu, Some(4));
        assert!(!template.retired);
        assert_eq!(
            template.sources,
            [TemplateSource {
                repo: Some("Trilogy-Care/tc-supplier-portal-v2".into()),
                path: "/workspace/tc-supplier-portal-v2".into(),
            }]
        );
    }

    #[test]
    fn a_template_with_one_golden_key_anywhere_gives_it_and_with_two_gives_none() {
        let fake = FakeVims::new();
        fake.on(
            "templates show",
            Reply::ok(r#"{"name":"t","current":{"build":{"key":"golden:one"}}}"#),
        )
        .on(
            "templates show",
            Reply::ok(r#"{"name":"t","builds":[{"key":"golden:one"},{"key":"golden:two"}]}"#),
        )
        .on(
            "templates show",
            Reply::ok(r#"{"name":"t","status":"retired"}"#),
        );
        assert_eq!(
            fake.template("t").unwrap().source_key.as_deref(),
            Some("golden:one")
        );
        assert_eq!(fake.template("t").unwrap().source_key, None);
        assert!(fake.template("t").unwrap().retired);
    }

    #[test]
    fn connections_are_listed() {
        let fake = FakeVims::new();
        fake.on(
            "connection list",
            Reply::ok(r#"{"connections":[{"id":"conn_1","provider":"github","name":"Trilogy GitHub"}]}"#),
        );
        assert_eq!(
            fake.connections().unwrap(),
            [Connection {
                id: "conn_1".into(),
                provider: Some("github".into()),
                name: Some("Trilogy GitHub".into()),
            }]
        );
    }

    #[test]
    fn github_gives_the_login_for_an_owner() {
        let fake = FakeVims::new();
        fake.on(
            "github",
            Reply::ok(r#"{"installations":[{"account":{"login":"Trilogy-Care"},"login":"tomkaczocha"}]}"#),
        )
        .on("github", Reply::ok(r#"{"login":"tomkaczocha"}"#));
        let links = fake.github().unwrap();
        assert_eq!(links.login_for("trilogy-care"), Some("tomkaczocha"));
        assert_eq!(links.login_for("rawphp"), None);
        let one = fake.github().unwrap();
        assert_eq!(one.login_for("anyone"), Some("tomkaczocha"));
    }

    #[test]
    fn the_profile_says_whether_it_holds_a_skill() {
        let fake = FakeVims::new();
        fake.on(
            "profile show",
            Reply::ok(r#"{"profile":{"skills":[{"name":"factory-build"},{"name":"other"}]}}"#),
        )
        .on("profile show", Reply::ok(r#"{"skills":["other"]}"#))
        .on("profile show", Reply::ok(r#"{"claude":{"skills":["factory-build"]}}"#));
        assert!(fake.profile_has_skill("factory-build").unwrap());
        assert!(!fake.profile_has_skill("factory-build").unwrap());
        assert!(fake.profile_has_skill("factory-build").unwrap());
    }

    #[test]
    fn an_answer_that_cannot_be_read_is_unknown_and_names_the_command() {
        let fake = FakeVims::new();
        fake.on("operation show", Reply::ok("not json"))
            .on("new", Reply::ok(r#"{"status":"running"}"#));
        let error = fake.operation("op_1").unwrap_err();
        assert!(matches!(error, VimsError::Unknown { .. }), "{error:?}");
        assert!(
            error.to_string().contains("vims operation show op_1 --json"),
            "{error}"
        );
        assert!(matches!(
            fake.new_workspace("golden:abc", Egress::Open, "k1"),
            Err(VimsError::Unknown { .. })
        ));
    }
}

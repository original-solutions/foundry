//! Factory records, factory settings and run ids.

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ids::{MachineId, ProjectId, SessionId, WorkspaceId};
    use crate::{assert_round_trip, parse_error};

    fn run(text: &str) -> RunId {
        text.parse().expect(text)
    }

    fn new_run(repo: &str, issue: u64, attempt: u32) -> RunId {
        RunId::new(repo, issue, attempt).expect(repo)
    }

    // The runner's `src/ledger/runId.test.ts` at 8ed9bef, for the numeric issues a GitHub
    // repo has.

    #[test]
    fn run_id_converts_to_and_from_its_string() {
        let id = RunId {
            repo: "rawphp/softfac".into(),
            issue: 8,
            attempt: 1,
        };
        assert_eq!(id.to_string(), "rawphp__softfac__8__1");
        assert_eq!(run("rawphp__softfac__8__1"), id);
        assert_eq!(new_run("rawphp/softfac", 8, 1), id);
    }

    #[test]
    fn run_id_round_trips_dots_dashes_and_large_numbers() {
        let id = new_run("my-org/my.repo-2", u64::MAX, u32::MAX);
        assert_eq!(
            id.to_string(),
            "my-org__my.repo-2__18446744073709551615__4294967295"
        );
        assert_eq!(run(&id.to_string()), id);
    }

    #[test]
    fn run_id_new_rejects_bad_repos() {
        for repo in [
            "",
            "softfac",
            "rawphp/",
            "/softfac",
            "a/b/c",
            "a\\b/c",
            "a/b c",
            "a/b\\c",
            "a/b\0",
            "a/b\u{1b}",
            "a/bé",
            "a/b~",
            "a/b:c",
            "a/b*",
            "../x",
            "x/..",
            "./x",
            "../../etc/passwd",
            "a b/c",
            "_tom/x",
            "tom_/x",
            "to__m/x",
            "tom_tc_/x",
            "my.org/x",
            "a/.",
            "a/b\n",
            "a\0/b",
            "raé/b",
        ] {
            let error = RunId::new(repo, 1, 1).expect_err(repo);
            assert!(error.contains("repo"), "{repo:?}: {error}");
        }
    }

    #[test]
    fn run_id_round_trips_an_owner_with_an_interior_underscore() {
        for (repo, text) in [
            ("tom_tc/softfac", "tom_tc__softfac__8__1"),
            ("tom_tc/a__b", "tom_tc__a__b__8__1"),
            ("a_b_c/_x_", "a_b_c___x___8__1"),
        ] {
            let id = new_run(repo, 8, 1);
            assert_eq!(id.repo, repo);
            assert_eq!(id.to_string(), text);
            assert_eq!(run(text), id);
        }
    }

    #[test]
    fn run_ids_for_awkward_owners_and_repos_are_distinct_and_parse_back() {
        let combos: [(&str, &str, u64, u32); 16] = [
            ("tom_tc", "softfac", 8, 1),
            ("tom_tc", "a__b", 8, 1),
            ("tom", "_x", 8, 1),
            ("tom", "x_", 8, 1),
            ("tom", "__", 8, 1),
            ("tom", "_", 8, 1),
            ("tom", "8__1", 8, 1),
            ("tom", "8", 1, 8),
            ("tom", "8__1", 1, 8),
            ("tom_8", "1__2", 3, 4),
            ("tom", "1__2", 3, 4),
            ("a_b_c", "d_e__f_", 12, 34),
            ("a", "b__c", 1, 1),
            ("a_b", "c", 1, 1),
            ("a-b", "c.d-e_f", 0, 0),
            ("x", "y", u64::MAX, u32::MAX),
        ];
        let mut seen = std::collections::BTreeSet::new();
        for (owner, name, issue, attempt) in combos {
            let id = new_run(&format!("{owner}/{name}"), issue, attempt);
            let text = id.to_string();
            assert_eq!(run(&text), id, "{text}");
            assert!(seen.insert(text.clone()), "{text} is not distinct");
        }
        assert_eq!(seen.len(), combos.len());
    }

    #[test]
    fn run_id_round_trips_underscores_in_the_repo_name() {
        for (repo, text) in [
            ("rawphp/my_app", "rawphp__my_app__8__1"),
            ("rawphp/_private", "rawphp___private__8__1"),
            ("rawphp/trailing_", "rawphp__trailing___8__1"),
            ("rawphp/a__b", "rawphp__a__b__8__1"),
            ("rawphp/a__8", "rawphp__a__8__8__1"),
            ("rawphp/__", "rawphp______8__1"),
            ("rawphp/softfac__8__1", "rawphp__softfac__8__1__8__1"),
        ] {
            let id = new_run(repo, 8, 1);
            assert_eq!(id.repo, repo);
            assert_eq!(id.to_string(), text, "{repo}");
            assert_eq!(run(text), id, "{repo}");
        }
    }

    #[test]
    fn run_id_lowercases_so_one_repo_is_one_id_and_one_file() {
        let mixed = new_run("Rawphp/SoftFac", 8, 1);
        let lower = new_run("rawphp/softfac", 8, 1);
        assert_eq!(mixed, lower);
        assert_eq!(mixed.repo, "rawphp/softfac");
        assert_eq!(mixed.to_string(), "rawphp__softfac__8__1");
        assert_eq!(run("Rawphp__SoftFac__8__1"), lower);
        assert_eq!(
            new_run("Rawphp/My__App", 8, 1).to_string(),
            "rawphp__my__app__8__1"
        );
        assert_eq!(
            run("RAWPHP__My__App__8__1"),
            new_run("rawphp/my__app", 8, 1)
        );
    }

    #[test]
    fn run_id_lowercases_ascii_only_so_a_lookalike_capital_is_not_folded_into_a_legal_name() {
        // U+212A KELVIN SIGN lowercases to an ASCII `k` under Unicode rules.
        assert!(RunId::new("\u{212A}elvin/x", 1, 1).is_err());
        assert!("\u{212A}elvin__x__1__1".parse::<RunId>().is_err());
    }

    #[test]
    fn a_run_id_that_is_not_the_shape_is_refused() {
        for text in [
            "",
            "rawphp",
            "rawphp__softfac",
            "rawphp__softfac__8",
            "rawphp__softfac__08__1",
            "rawphp__softfac__8__01",
            "rawphp__softfac__+8__1",
            "rawphp__softfac__-8__1",
            "rawphp__softfac__8__-1",
            "rawphp__softfac__eng-42__1",
            "rawphp__softfac__ENG-42__1",
            "rawphp__softfac__8__4294967296",
            "rawphp__softfac__18446744073709551616__1",
            "rawphp__softfac__8__1.5",
            "__softfac__8__1",
            "rawphp____8__1",
            "rawphp__..__8__1",
            "rawphp__.__8__1",
            "raw.php__softfac__8__1",
            "rawphp__soft fac__8__1",
            "rawphp__soft/fac__8__1",
        ] {
            assert!(text.parse::<RunId>().is_err(), "{text:?}");
            parse_error::<RunId>(&serde_json::to_string(text).unwrap());
        }
    }

    #[test]
    fn a_run_id_is_written_as_its_string() {
        let id = new_run("original-solutions/foundry", 129, 2);
        assert_eq!(
            serde_json::to_value(&id).unwrap(),
            "original-solutions__foundry__129__2"
        );
        assert_round_trip(&id);
        let read: RunId = serde_json::from_str("\"original-solutions__foundry__129__2\"").unwrap();
        assert_eq!(read, id);
    }

    #[test]
    fn a_hand_built_run_id_that_is_not_in_its_lowercase_form_cannot_be_written() {
        for repo in ["Rawphp/softfac", "../x", "a/b/c", "softfac"] {
            let id = RunId {
                repo: repo.into(),
                issue: 1,
                attempt: 1,
            };
            assert!(serde_json::to_string(&id).is_err(), "{repo}");
        }
    }

    // Factory records.

    /// Lines of the runner's `src/ledger/ledger.test.ts` at 8ed9bef: one with a cause, one
    /// without, and one whose data holds a nested object, a null and a negative number.
    const LEDGER_LINES: [&str; 3] = [
        r#"{"ts":"2026-10-04T10:13:16.123Z","ms":1791108796123,"event":"step_changed","data":{"step":"red"},"cause":{"step":"pass","reason":"claimed"}}"#,
        r#"{"ts":"2026-10-04T10:13:16.123Z","ms":1791108796123,"event":"step_changed","data":{"step":"red"}}"#,
        r#"{"ts":"2026-10-04T10:13:16.123Z","ms":1791108796123,"event":"gate_command","data":{"attempt":1,"command":"cargo test","exit_code":-1,"duration_ms":4200,"extra":null},"cause":{"step":"gate","reason":null}}"#,
    ];

    fn event_from_line(line: &str, run: RunId, generation: u64) -> RunEvent {
        let line: serde_json::Value = serde_json::from_str(line).unwrap();
        RunEvent {
            run,
            generation,
            ts: line["ts"].as_str().unwrap().to_owned(),
            ms: line["ms"].as_u64().unwrap(),
            event: line["event"].as_str().unwrap().to_owned(),
            data: line["data"].clone(),
            cause: line.get("cause").cloned(),
        }
    }

    pub(crate) fn run_event() -> RunEvent {
        event_from_line(
            LEDGER_LINES[0],
            new_run("original-solutions/foundry", 129, 1),
            3,
        )
    }

    #[test]
    fn a_run_event_serialises_to_the_keys_of_a_ledger_line_beside_its_run_and_generation() {
        for line in LEDGER_LINES {
            let id = new_run("rawphp/softfac", 8, 1);
            let event = event_from_line(line, id.clone(), 7);
            let mut json = serde_json::to_value(&event).unwrap();
            let object = json.as_object_mut().unwrap();
            assert_eq!(object.remove("run").unwrap(), "rawphp__softfac__8__1");
            assert_eq!(object.remove("generation").unwrap(), 7);
            let expected: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(json, expected, "{line}");
            assert_round_trip(&event);
        }
    }

    #[test]
    fn a_run_event_reads_a_ledger_line_with_its_run_and_generation() {
        let line = LEDGER_LINES[1];
        let json = format!(
            r#"{{"run":"rawphp__softfac__8__1","generation":2,{}"#,
            &line[1..]
        );
        let read: RunEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(
            read,
            event_from_line(line, new_run("rawphp/softfac", 8, 1), 2)
        );
        assert_eq!(read.cause, None);
    }

    pub(crate) fn status() -> FactoryStatus {
        let id = new_run("original-solutions/foundry", 129, 1);
        let other = new_run("original-solutions/foundry", 130, 2);
        FactoryStatus {
            project: ProjectId::random(),
            pass: 41,
            holds: true,
            generation: 3,
            not_holding: None,
            idle: Some("`gh` failed three passes running".into()),
            owner_ignored: true,
            ready: vec![Waiting {
                issue: 131,
                title: "feat: the clock".into(),
                reason: "no room".into(),
                run: None,
            }],
            feedback_waiting: vec![Waiting {
                issue: 130,
                title: "feat: the store".into(),
                reason: "every run slot is held".into(),
                run: Some(other.clone()),
            }],
            runs: vec![LiveRun {
                run: id.clone(),
                title: "feat: the shared types".into(),
                step: "building".into(),
                machine: MachineId::random(),
                gate_retries_used: 1,
                minutes_used: 42,
                workspaces: vec![WorkspaceId::random(), WorkspaceId::random()],
                sessions: vec![SessionId::random()],
            }],
            held_up: vec![
                HeldUp {
                    run: other.clone(),
                    kind: HeldUpKind::Stuck,
                    reason: "the gate failed".into(),
                },
                HeldUp {
                    run: id.clone(),
                    kind: HeldUpKind::NeedsInfo,
                    reason: "1. Which login?".into(),
                },
            ],
            gate_infra: vec![other.clone()],
            finished: vec![FinishedRun {
                run: new_run("original-solutions/foundry", 99, 1),
                title: "fix: a typo".into(),
                outcome: "pr-open".into(),
                pr_end: Some("merged".into()),
                pr_url: Some("https://github.com/original-solutions/foundry/pull/7".into()),
            }],
            foreign: vec![ForeignClaim {
                issue: 12,
                title: "chore: claimed by the runner".into(),
                state: "building".into(),
            }],
        }
    }

    pub(crate) fn held() -> MachineHeld {
        MachineHeld {
            logins: vec!["tomkaczocha".into(), "rawphp".into()],
            skill: true,
        }
    }

    pub(crate) fn turn() -> SessionTurn {
        SessionTurn {
            session: SessionId::random(),
            workspace: WorkspaceId::random(),
            project: ProjectId::random(),
            turn: 4,
            prompts: 5,
            question: true,
        }
    }

    pub(crate) fn records() -> Vec<FactoryRecord> {
        let owner = MachineId::random();
        vec![
            FactoryRecord {
                owner,
                version: 1,
                body: FactoryBody::RunEvent(run_event()),
            },
            FactoryRecord {
                owner,
                version: 2,
                body: FactoryBody::Status(status()),
            },
            FactoryRecord {
                owner,
                version: 3,
                body: FactoryBody::Held(held()),
            },
            FactoryRecord {
                owner,
                version: 4,
                body: FactoryBody::Turn(turn()),
            },
        ]
    }

    #[test]
    fn every_factory_type_round_trips() {
        let status = status();
        assert_round_trip(&run_event());
        assert_round_trip(&status);
        assert_round_trip(&held());
        assert_round_trip(&turn());
        for waiting in status.ready.iter().chain(&status.feedback_waiting) {
            assert_round_trip(waiting);
        }
        assert_round_trip(&status.runs[0]);
        for held_up in &status.held_up {
            assert_round_trip(held_up);
            assert_round_trip(&held_up.kind);
        }
        assert_round_trip(&status.finished[0]);
        assert_round_trip(&status.foreign[0]);
        for not_holding in [
            NotHolding::NoToken {
                login: "rawphp".into(),
            },
            NotHolding::HeldBy {
                machine: MachineId::random(),
                generation: 9,
            },
        ] {
            assert_round_trip(&not_holding);
            assert_round_trip(&FactoryStatus {
                holds: false,
                not_holding: Some(not_holding),
                ..status.clone()
            });
        }
        assert_round_trip(&FactoryStatus {
            idle: None,
            ready: vec![],
            feedback_waiting: vec![],
            runs: vec![],
            held_up: vec![],
            gate_infra: vec![],
            finished: vec![],
            foreign: vec![],
            ..status
        });
        for record in records() {
            assert_round_trip(&record);
        }
    }

    #[test]
    fn a_factory_record_is_written_with_its_kind_beside_its_owner_and_version() {
        let kinds = [
            "run_event",
            "factory_status",
            "machine_held",
            "session_turn",
        ];
        for (record, kind) in records().into_iter().zip(kinds) {
            let json = serde_json::to_value(&record).unwrap();
            assert_eq!(json["kind"], kind, "{json}");
            assert_eq!(json["owner"], record.owner.to_string());
            assert_eq!(json["version"], record.version);
            assert!(json["body"].is_object(), "{json}");
            assert_eq!(record.body.kind(), kind);
            assert_eq!(record.key().kind, kind);
        }
    }

    #[test]
    fn held_up_and_not_holding_are_written_under_their_names() {
        assert_eq!(
            serde_json::to_value(HeldUpKind::NeedsInfo).unwrap(),
            "needs_info"
        );
        assert_eq!(serde_json::to_value(HeldUpKind::Stuck).unwrap(), "stuck");
        let machine = "67e55044-10b1-426f-9247-bb680e5fe0c8";
        let cases = [
            (
                r#"{"kind":"no_token","login":"rawphp"}"#.to_owned(),
                NotHolding::NoToken {
                    login: "rawphp".into(),
                },
            ),
            (
                format!(r#"{{"kind":"held_by","machine":"{machine}","generation":4}}"#),
                NotHolding::HeldBy {
                    machine: MachineId(machine.parse().unwrap()),
                    generation: 4,
                },
            ),
        ];
        for (json, expected) in cases {
            assert_eq!(serde_json::from_str::<NotHolding>(&json).unwrap(), expected);
            assert_eq!(serde_json::to_string(&expected).unwrap(), json);
        }
    }

    #[test]
    fn a_factory_record_reads_from_its_wire_form() {
        let machine = "67e55044-10b1-426f-9247-bb680e5fe0c8";
        let session = "1b4e28ba-2fa1-11d2-883f-0016d3cca427";
        let workspace = "22222222-2222-4222-8222-222222222222";
        let project = "11111111-1111-4111-8111-111111111111";
        let owner = MachineId(machine.parse().unwrap());
        let cases = [
            (
                format!(
                    r#"{{"kind":"machine_held","owner":"{machine}","version":3,"body":{{"logins":["rawphp"],"skill":false}}}}"#
                ),
                FactoryRecord {
                    owner,
                    version: 3,
                    body: FactoryBody::Held(MachineHeld {
                        logins: vec!["rawphp".into()],
                        skill: false,
                    }),
                },
            ),
            (
                format!(
                    r#"{{"kind":"session_turn","owner":"{machine}","version":4,"body":{{"session":"{session}","workspace":"{workspace}","project":"{project}","turn":2,"prompts":3,"question":false}}}}"#
                ),
                FactoryRecord {
                    owner,
                    version: 4,
                    body: FactoryBody::Turn(SessionTurn {
                        session: SessionId(session.parse().unwrap()),
                        workspace: WorkspaceId(workspace.parse().unwrap()),
                        project: ProjectId(project.parse().unwrap()),
                        turn: 2,
                        prompts: 3,
                        question: false,
                    }),
                },
            ),
            (
                format!(
                    r#"{{"kind":"run_event","owner":"{machine}","version":5,"body":{{"run":"rawphp__softfac__8__1","generation":1,"ts":"t","ms":0,"event":"claimed","data":{{"machine":"m","github_user":"rawphp"}}}}}}"#
                ),
                FactoryRecord {
                    owner,
                    version: 5,
                    body: FactoryBody::RunEvent(RunEvent {
                        run: new_run("rawphp/softfac", 8, 1),
                        generation: 1,
                        ts: "t".into(),
                        ms: 0,
                        event: "claimed".into(),
                        data: serde_json::json!({"machine": "m", "github_user": "rawphp"}),
                        cause: None,
                    }),
                },
            ),
            (
                format!(
                    r#"{{"kind":"factory_status","owner":"{machine}","version":6,"body":{{"project":"{project}","pass":1,"holds":false,"generation":0,"not_holding":{{"kind":"no_token","login":"rawphp"}},"idle":null,"owner_ignored":false,"ready":[],"feedback_waiting":[],"runs":[],"held_up":[],"gate_infra":[],"finished":[],"foreign":[]}}}}"#
                ),
                FactoryRecord {
                    owner,
                    version: 6,
                    body: FactoryBody::Status(FactoryStatus {
                        project: ProjectId(project.parse().unwrap()),
                        pass: 1,
                        holds: false,
                        generation: 0,
                        not_holding: Some(NotHolding::NoToken {
                            login: "rawphp".into(),
                        }),
                        idle: None,
                        owner_ignored: false,
                        ready: vec![],
                        feedback_waiting: vec![],
                        runs: vec![],
                        held_up: vec![],
                        gate_infra: vec![],
                        finished: vec![],
                        foreign: vec![],
                    }),
                },
            ),
        ];
        for (json, expected) in cases {
            assert_eq!(
                serde_json::from_str::<FactoryRecord>(&json).expect(&json),
                expected
            );
        }
    }

    #[test]
    fn an_unknown_kind_of_factory_record_is_an_error_that_names_it() {
        let machine = "67e55044-10b1-426f-9247-bb680e5fe0c8";
        let error = parse_error::<FactoryRecord>(&format!(
            r#"{{"kind":"workspace","owner":"{machine}","version":1,"body":{{}}}}"#
        ));
        assert!(error.contains("workspace"), "{error}");
    }

    #[test]
    fn each_record_is_keyed_by_its_kind_and_the_id_that_makes_it_one_record() {
        let owner = MachineId::random();
        let other = MachineId::random();
        let event = run_event();
        let status = status();
        let turn = turn();
        let record = |owner, version, body| FactoryRecord {
            owner,
            version,
            body,
        };

        // One record per event: two events of a run, by one owner or two, never share a key.
        let first = record(owner, 1, FactoryBody::RunEvent(event.clone()));
        let second = record(owner, 2, FactoryBody::RunEvent(event.clone()));
        let elsewhere = record(other, 1, FactoryBody::RunEvent(event.clone()));
        assert_ne!(first.key(), second.key());
        assert_ne!(first.key(), elsewhere.key());
        assert!(first.key().id.starts_with(&event.run.to_string()));

        // One status per project and writer, rewritten under new versions.
        let mine = record(owner, 1, FactoryBody::Status(status.clone()));
        let again = record(owner, 9, FactoryBody::Status(status.clone()));
        let theirs = record(other, 1, FactoryBody::Status(status.clone()));
        let next_project = record(
            owner,
            1,
            FactoryBody::Status(FactoryStatus {
                project: ProjectId::random(),
                ..status
            }),
        );
        assert_eq!(mine.key(), again.key());
        assert_ne!(mine.key(), theirs.key());
        assert_ne!(mine.key(), next_project.key());

        // One `MachineHeld` per machine.
        let held_mine = record(owner, 1, FactoryBody::Held(held()));
        let held_again = record(owner, 5, FactoryBody::Held(MachineHeld::default()));
        let held_theirs = record(other, 1, FactoryBody::Held(held()));
        assert_eq!(held_mine.key(), held_again.key());
        assert_ne!(held_mine.key(), held_theirs.key());
        assert_eq!(held_mine.key().id, owner.to_string());

        // One `SessionTurn` per session.
        let turn_one = record(owner, 1, FactoryBody::Turn(turn.clone()));
        let turn_two = record(
            owner,
            2,
            FactoryBody::Turn(SessionTurn {
                turn: 5,
                ..turn.clone()
            }),
        );
        assert_eq!(turn_one.key(), turn_two.key());
        assert_eq!(turn_one.key().id, turn.session.to_string());

        // Kinds never share a key.
        let keys = [first.key(), mine.key(), held_mine.key(), turn_one.key()];
        for (i, a) in keys.iter().enumerate() {
            for b in &keys[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn an_unknown_field_is_ignored() {
        for record in records() {
            let mut json = serde_json::to_value(&record).unwrap();
            json["added_later"] = serde_json::json!(1);
            json["body"]["added_later"] = serde_json::json!("x");
            let back: FactoryRecord = serde_json::from_value(json).unwrap();
            assert_eq!(back, record);
        }
    }
}

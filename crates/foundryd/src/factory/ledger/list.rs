//! Listing the runs in the ledger.

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use foundry_proto::factory::{PrEnd, RunId};
    use foundry_proto::ids::MachineId;
    use serde_json::json;

    use super::*;
    use crate::factory::ledger::Ledger;
    use crate::factory::ledger::events::{Event, Outcome};
    use crate::factory::ledger::testing::*;
    use crate::factory::lifecycle::State;
    use crate::store::Store;

    fn rid(issue: u64, repo: &str, attempt: u32) -> RunId {
        RunId::new(repo, issue, attempt).unwrap()
    }

    fn softfac(issue: u64) -> RunId {
        rid(issue, "rawphp/softfac", 1)
    }

    fn claimed() -> Event {
        Event::Claimed {
            machine: "m".into(),
            github_user: "u".into(),
        }
    }

    fn moved(from: State, to: State) -> Event {
        Event::StateChanged { from, to }
    }

    fn launched(title: Option<&str>, base: Option<&str>) -> Event {
        Event::SessionLaunched {
            thread_id: "t-1".into(),
            worktree: Some("/wt".into()),
            branch: Some("b".into()),
            model_instance: Some("m".into()),
            model: Some("x".into()),
            model_options: None,
            resumed_from: None,
            base: base.map(str::to_owned),
            title: title.map(str::to_owned),
        }
    }

    fn asked(questions: &str) -> Event {
        Event::QuestionsPosted {
            questions: questions.into(),
        }
    }

    /// A machine's ledger over `store` whose clock starts at `start` and ticks a second a read.
    fn at(store: &Arc<Mutex<Store>>, start: u64) -> Ledger {
        ledger(store, MachineId::random(), 1, ticking(start, 1000))
    }

    /// A run started at `start_ms` whose last state is `to`, finished when `finish`.
    fn run_at(
        store: &Arc<Mutex<Store>>,
        id: &RunId,
        start_ms: u64,
        to: Option<State>,
        finish: bool,
        approved: bool,
    ) -> Ledger {
        let ledger = at(store, start_ms);
        if approved {
            ledger
                .write(id, &step("approved 2026-10-04T10:00:00Z"), &test_cause())
                .unwrap();
        }
        ledger.write(id, &claimed(), &test_cause()).unwrap();
        if let Some(to) = to {
            ledger
                .write(id, &moved(State::Ready, to), &test_cause())
                .unwrap();
        }
        if finish {
            ledger
                .finish(id, Outcome::Stuck, None, None, &test_cause(), None)
                .unwrap();
        }
        ledger
    }

    /// A run event row this build cannot read, as a newer build may leave one.
    fn unreadable_run(dir: &Path, id: &RunId) {
        let owner = MachineId::random();
        let conn = rusqlite::Connection::open(dir.join("foundry.db")).unwrap();
        conn.execute(
            "INSERT INTO factory_records (kind, id, owner, version, record)
             VALUES ('run_event', ?1, ?2, 1, '{\"kind\":\"run_event\",\"later\":true}')",
            rusqlite::params![format!("{id}/{owner}/1"), owner.to_string()],
        )
        .unwrap();
    }

    fn info(id: RunId, state: Option<State>, confirmed: bool) -> RunInfo {
        RunInfo {
            id,
            state,
            unreadable: false,
            confirmed,
            session: false,
            pr_reported: false,
            base: None,
        }
    }

    fn listed(store: &Arc<Mutex<Store>>, repo: &str) -> Vec<RunInfo> {
        runs(&store.lock().unwrap(), repo).unwrap()
    }

    fn summaries_of(store: &Arc<Mutex<Store>>, repo: &str) -> Vec<RunSummary> {
        summaries(&store.lock().unwrap(), repo).unwrap()
    }

    // The runner's `listRuns.test.ts`: `listUnfinishedRuns` is `runs`. The cases about the
    // runs directory and reading a file from its ends are the store's now.

    #[test]
    fn an_empty_data_dir_has_no_runs() {
        let (_dir, store) = open_store();
        assert_eq!(listed(&store, "rawphp/softfac"), []);
    }

    #[test]
    fn lists_the_unfinished_runs_oldest_first_with_their_state_and_without_the_finished_ones() {
        // Nothing orders records by wall-clock time: runs are listed in run id order, the issue
        // and attempt as numbers. Issue numbers rise as issues are filed.
        let (_dir, store) = open_store();
        run_at(&store, &softfac(3), T0 + 20_000, Some(State::Building), false, true);
        run_at(&store, &softfac(1), T0, Some(State::Triage), false, true);
        run_at(&store, &softfac(2), T0 + 10_000, Some(State::PrOpen), true, true);
        run_at(&store, &softfac(4), T0 + 30_000, None, false, true);
        assert_eq!(
            listed(&store, "rawphp/softfac"),
            [
                info(softfac(1), Some(State::Triage), true),
                info(softfac(3), Some(State::Building), true),
                info(softfac(4), None, true),
            ]
        );
    }

    #[test]
    fn a_run_that_ended_in_needs_info_is_not_listed() {
        let (_dir, store) = open_store();
        run_at(&store, &softfac(5), T0, Some(State::NeedsInfo), true, true);
        assert_eq!(listed(&store, "rawphp/softfac"), []);
    }

    #[test]
    fn a_run_whose_file_cannot_be_read_is_still_listed_flagged_unreadable_after_the_others() {
        let (dir, store) = open_store();
        run_at(&store, &softfac(7), T0 + 5_000, Some(State::Building), false, true);
        unreadable_run(dir.path(), &softfac(6));
        assert_eq!(
            listed(&store, "rawphp/softfac"),
            [
                info(softfac(7), Some(State::Building), true),
                RunInfo {
                    unreadable: true,
                    ..info(softfac(6), None, false)
                },
            ]
        );
    }

    #[test]
    fn a_run_with_an_unknown_last_state_is_listed_with_no_state() {
        let (_dir, store) = open_store();
        put(
            &store,
            MachineId::random(),
            1,
            &softfac(9),
            1,
            T0,
            "state_changed",
            json!({"from": "ready", "to": "later"}),
            None,
        );
        assert_eq!(
            listed(&store, "rawphp/softfac"),
            [info(softfac(9), None, false)]
        );
    }

    #[test]
    fn a_run_with_no_approval_and_no_session_is_listed_as_not_confirmed() {
        let (_dir, store) = open_store();
        run_at(&store, &softfac(8), T0, Some(State::Triage), false, false);
        assert_eq!(
            listed(&store, "rawphp/softfac"),
            [info(softfac(8), Some(State::Triage), false)]
        );
    }

    #[test]
    fn with_one_issue_named_lists_only_that_issues_runs_whatever_the_repos_case() {
        let (dir, store) = open_store();
        run_at(&store, &softfac(1), T0, Some(State::Triage), false, true);
        run_at(
            &store,
            &rid(1, "rawphp/softfac", 2),
            T0 + 1_000,
            Some(State::Building),
            false,
            true,
        );
        run_at(&store, &softfac(2), T0 + 2_000, Some(State::Triage), false, true);
        run_at(
            &store,
            &rid(1, "rawphp/other", 1),
            T0 + 3_000,
            Some(State::Triage),
            false,
            true,
        );
        // A run of another issue that cannot be read is not read, so it is not listed.
        unreadable_run(dir.path(), &softfac(3));
        assert_eq!(
            issue_runs(&store.lock().unwrap(), "RawPhp/Softfac", 1).unwrap(),
            [
                info(softfac(1), Some(State::Triage), true),
                info(rid(1, "rawphp/softfac", 2), Some(State::Building), true),
            ]
        );
    }

    #[test]
    fn the_runs_of_another_repo_are_not_listed_and_the_repo_is_matched_in_any_case() {
        let (_dir, store) = open_store();
        run_at(&store, &softfac(1), T0, Some(State::Triage), false, true);
        run_at(
            &store,
            &rid(1, "rawphp/softfac__x", 1),
            T0,
            Some(State::Triage),
            false,
            true,
        );
        assert_eq!(
            listed(&store, "RawPHP/SoftFac"),
            [info(softfac(1), Some(State::Triage), true)]
        );
        assert_eq!(listed(&store, "not a repo"), []);
    }

    #[test]
    fn is_set_by_a_launched_session_or_a_launch_that_was_sent_and_not_by_the_claim_alone() {
        let (_dir, store) = open_store();
        run_at(&store, &softfac(1), T0, Some(State::Triage), false, true);
        run_at(&store, &softfac(2), T0 + 10_000, Some(State::Triage), false, true);
        run_at(&store, &softfac(3), T0 + 20_000, Some(State::Triage), false, true);
        run_at(&store, &softfac(4), T0 + 20_000, Some(State::Triage), false, false);
        let later = at(&store, T0 + 30_000);
        later
            .write(&softfac(2), &step("launch-sending"), &test_cause())
            .unwrap();
        later
            .write(&softfac(3), &launched(None, None), &test_cause())
            .unwrap();
        later
            .write(
                &softfac(4),
                &step("launch-sending resumed-from fac/4-1"),
                &test_cause(),
            )
            .unwrap();
        let sessions: Vec<(u64, bool, bool)> = listed(&store, "rawphp/softfac")
            .into_iter()
            .map(|run| (run.id.issue(), run.session, run.confirmed))
            .collect();
        // A launched session confirms a run that recorded no approval; a launch sent does not.
        assert_eq!(
            sessions,
            [(1, false, true), (2, true, true), (3, true, true), (4, true, false)]
        );
    }

    #[test]
    fn a_run_is_listed_with_the_base_its_newest_launch_recorded() {
        let (_dir, store) = open_store();
        let ledger = run_at(&store, &softfac(1), T0, Some(State::Building), false, false);
        ledger
            .write(&softfac(1), &launched(None, Some("release")), &test_cause())
            .unwrap();
        ledger
            .write(&softfac(1), &launched(None, None), &test_cause())
            .unwrap();
        assert_eq!(
            listed(&store, "rawphp/softfac"),
            [RunInfo {
                session: true,
                base: Some("release".into()),
                ..info(softfac(1), Some(State::Building), true)
            }]
        );
    }

    #[test]
    fn is_listed_from_pr_opened_until_the_run_is_building_again() {
        let (_dir, store) = open_store();
        let opened = run_at(&store, &softfac(1), T0, Some(State::Building), false, true);
        opened
            .write(
                &softfac(1),
                &Event::PrOpened {
                    pr: 12,
                    url: "https://x/pull/12".into(),
                },
                &test_cause(),
            )
            .unwrap();
        let reported = |store: &Arc<Mutex<Store>>| -> Vec<bool> {
            listed(store, "rawphp/softfac")
                .into_iter()
                .map(|run| run.pr_reported)
                .collect()
        };
        assert_eq!(reported(&store), [true]);
        opened
            .write(
                &softfac(1),
                &moved(State::Building, State::PrOpen),
                &test_cause(),
            )
            .unwrap();
        opened
            .write(
                &softfac(1),
                &moved(State::PrOpen, State::Building),
                &test_cause(),
            )
            .unwrap();
        assert_eq!(reported(&store), [false]);
        // A gate that sends the run back puts its session to work again.
        opened
            .write(
                &softfac(1),
                &Event::PrOpened {
                    pr: 12,
                    url: "https://x/pull/12".into(),
                },
                &test_cause(),
            )
            .unwrap();
        assert_eq!(reported(&store), [true]);
        opened
            .write(
                &softfac(1),
                &Event::GateSentBack {
                    attempt: 1,
                    run_ordinal: 2,
                },
                &test_cause(),
            )
            .unwrap();
        assert_eq!(reported(&store), [false]);
    }

    #[test]
    fn every_confirmed_unfinished_run_holds_a_slot_but_one_waiting_in_pr_open() {
        assert!(holds_slot(&info(softfac(1), Some(State::Building), true)));
        assert!(holds_slot(&info(softfac(1), Some(State::Triage), true)));
        assert!(holds_slot(&info(softfac(1), None, true)));
        assert!(!holds_slot(&info(softfac(1), Some(State::PrOpen), true)));
    }

    #[test]
    fn a_run_that_is_not_confirmed_holds_no_slot_it_never_launches_a_session() {
        assert!(!holds_slot(&info(softfac(1), Some(State::Triage), false)));
        assert!(!holds_slot(&info(softfac(1), None, false)));
    }

    #[test]
    fn a_building_run_whose_session_reported_its_pull_request_holds_no_slot_it_waits_for_the_gate()
     {
        assert!(!holds_slot(&RunInfo {
            pr_reported: true,
            ..info(softfac(1), Some(State::Building), true)
        }));
    }

    #[test]
    fn a_run_whose_file_cannot_be_read_holds_a_slot() {
        assert!(holds_slot(&RunInfo {
            unreadable: true,
            ..info(softfac(1), None, false)
        }));
        assert!(holds_slot(&RunInfo {
            unreadable: true,
            ..info(softfac(1), Some(State::PrOpen), false)
        }));
    }

    // `listRuns`: one summary per run, finished runs included.

    fn summary(id: RunId) -> RunSummary {
        RunSummary {
            id,
            started_ts: None,
            last_event: None,
            title: None,
            reason: None,
            state: None,
            step: None,
            pr_url: None,
            pr_end: None,
            finished: false,
            total_ms: None,
            unreadable: false,
        }
    }

    #[test]
    fn an_empty_ledger_lists_no_runs() {
        let (_dir, store) = open_store();
        assert_eq!(summaries_of(&store, "rawphp/softfac"), []);
    }

    #[test]
    fn lists_one_summary_per_run_oldest_first_finished_runs_included() {
        let (_dir, store) = open_store();
        run_at(&store, &softfac(12), T0 + 60_000, None, false, true);
        run_at(&store, &softfac(8), T0, None, true, true);
        assert_eq!(
            summaries_of(&store, "rawphp/softfac"),
            [
                RunSummary {
                    started_ts: Some("2026-10-04T10:13:16.123Z".into()),
                    last_event: Some("finished".into()),
                    finished: true,
                    total_ms: Some(2000),
                    ..summary(softfac(8))
                },
                RunSummary {
                    started_ts: Some("2026-10-04T10:14:16.123Z".into()),
                    last_event: Some("claimed".into()),
                    step: Some("approved 2026-10-04T10:00:00Z".into()),
                    ..summary(softfac(12))
                },
            ]
        );
    }

    #[test]
    fn reads_the_state_from_the_last_state_change() {
        let (_dir, store) = open_store();
        let ledger = at(&store, T0);
        ledger
            .write(
                &softfac(8),
                &moved(State::Triage, State::Building),
                &test_cause(),
            )
            .unwrap();
        ledger
            .write(
                &softfac(8),
                &moved(State::Building, State::PrOpen),
                &test_cause(),
            )
            .unwrap();
        let read: Vec<(Option<State>, Option<String>)> = summaries_of(&store, "rawphp/softfac")
            .into_iter()
            .map(|run| (run.state, run.last_event))
            .collect();
        assert_eq!(read, [(Some(State::PrOpen), Some("state_changed".into()))]);
    }

    #[test]
    fn reads_the_step_from_the_last_step_changed_of_a_run_read_whole_null_before_any() {
        let (_dir, store) = open_store();
        let ledger = at(&store, T0);
        ledger.write(&softfac(8), &claimed(), &test_cause()).unwrap();
        assert_eq!(summaries_of(&store, "rawphp/softfac")[0].step, None);
        ledger
            .write(&softfac(8), &step("launch-sending"), &test_cause())
            .unwrap();
        ledger
            .write(&softfac(8), &step("gate-passed"), &test_cause())
            .unwrap();
        ledger.write(&softfac(8), &claimed(), &test_cause()).unwrap();
        assert_eq!(
            summaries_of(&store, "rawphp/softfac")[0].step.as_deref(),
            Some("gate-passed")
        );
    }

    #[test]
    fn a_finished_run_has_no_step_read_whole_or_from_the_ends_of_its_file() {
        let (_dir, store) = open_store();
        let ledger = at(&store, T0);
        ledger
            .write(&softfac(8), &step("gate-passed"), &test_cause())
            .unwrap();
        ledger
            .finish(&softfac(8), Outcome::Stuck, None, None, &test_cause(), None)
            .unwrap();
        let read = &summaries_of(&store, "rawphp/softfac")[0];
        assert!(read.finished);
        assert_eq!(read.step, None);
    }

    #[test]
    fn reads_the_pull_request_url_from_the_last_pr_opened_and_from_the_finished_line_over_it() {
        let (_dir, store) = open_store();
        let ledger = at(&store, T0);
        for pr in [1, 2] {
            ledger
                .write(
                    &softfac(8),
                    &Event::PrOpened {
                        pr,
                        url: format!("https://example.test/pull/{pr}"),
                    },
                    &test_cause(),
                )
                .unwrap();
        }
        assert_eq!(
            summaries_of(&store, "rawphp/softfac")[0].pr_url.as_deref(),
            Some("https://example.test/pull/2")
        );
        ledger
            .finish(
                &softfac(8),
                Outcome::PrOpen,
                None,
                Some("https://example.test/pull/3"),
                &test_cause(),
                Some(PrEnd::Merged),
            )
            .unwrap();
        assert_eq!(
            summaries_of(&store, "rawphp/softfac")[0].pr_url.as_deref(),
            Some("https://example.test/pull/3")
        );
    }

    #[test]
    fn a_pr_open_run_reads_finished_once_its_pull_request_ended_and_how_it_ended() {
        let (_dir, store) = open_store();
        let ledger = at(&store, T0);
        ledger
            .write(
                &softfac(8),
                &moved(State::Building, State::PrOpen),
                &test_cause(),
            )
            .unwrap();
        assert!(!summaries_of(&store, "rawphp/softfac")[0].finished);
        ledger
            .finish(
                &softfac(8),
                Outcome::PrOpen,
                None,
                Some("https://github.com/rawphp/softfac/pull/12"),
                &test_cause(),
                Some(PrEnd::Closed),
            )
            .unwrap();
        let read = &summaries_of(&store, "rawphp/softfac")[0];
        assert_eq!(read.state, Some(State::PrOpen));
        assert!(read.finished);
        assert_eq!(
            read.pr_url.as_deref(),
            Some("https://github.com/rawphp/softfac/pull/12")
        );
        assert_eq!(read.pr_end, Some(PrEnd::Closed));
    }

    #[test]
    fn an_unreadable_run_file_is_listed_with_no_record_and_the_others_still_are() {
        let (dir, store) = open_store();
        run_at(&store, &softfac(8), T0, Some(State::Building), false, true);
        unreadable_run(dir.path(), &softfac(2));
        let read = summaries_of(&store, "rawphp/softfac");
        let flags: Vec<(u64, bool)> = read
            .iter()
            .map(|run| (run.id.issue(), run.unreadable))
            .collect();
        assert_eq!(flags, [(8, false), (2, true)]);
        assert_eq!(
            read[1],
            RunSummary {
                unreadable: true,
                ..summary(softfac(2))
            }
        );
    }

    #[test]
    fn orders_runs_with_the_same_start_by_repo_then_issue_and_attempt_as_numbers() {
        let (_dir, store) = open_store();
        for id in [
            rid(10, "rawphp/softfac", 1),
            rid(9, "rawphp/softfac", 2),
            rid(9, "rawphp/softfac", 10),
            rid(9, "rawphp/softfac", 1),
        ] {
            run_at(&store, &id, T0, None, false, true);
        }
        let order: Vec<(u64, u32)> = summaries_of(&store, "rawphp/softfac")
            .into_iter()
            .map(|run| (run.id.issue(), run.id.attempt()))
            .collect();
        assert_eq!(order, [(9, 1), (9, 2), (9, 10), (10, 1)]);
    }

    /// A finished run of `id` with `events` after the claim.
    fn finished_run(store: &Arc<Mutex<Store>>, id: &RunId, events: &[Event], outcome: Outcome) {
        let ledger = at(store, T0 + id.issue() * 60_000);
        ledger.write(id, &claimed(), &test_cause()).unwrap();
        for event in events {
            ledger.write(id, event, &test_cause()).unwrap();
        }
        ledger
            .finish(id, outcome, None, None, &test_cause(), None)
            .unwrap();
    }

    fn title_and_reason(store: &Arc<Mutex<Store>>) -> Vec<(u64, Option<String>, Option<String>)> {
        summaries_of(store, "rawphp/softfac")
            .into_iter()
            .map(|run| (run.id.issue(), run.title, run.reason))
            .collect()
    }

    #[test]
    fn reads_the_title_of_the_first_launch_and_the_reason_the_last_state_asks_for_as_the_whole_read_does()
     {
        let (_dir, store) = open_store();
        // The first titled launch wins; the stuck line before the state change gives the reason.
        finished_run(
            &store,
            &softfac(1),
            &[
                launched(Some("Old title"), None),
                launched(Some("New title"), None),
                stuck("older"),
                stuck("gate failed"),
                moved(State::Building, State::Stuck),
            ],
            Outcome::Stuck,
        );
        // Needs info: the newest questions, whichever side of the state change they sit on.
        finished_run(
            &store,
            &softfac(2),
            &[
                asked("first?"),
                moved(State::Triage, State::NeedsInfo),
                asked("which db?"),
            ],
            Outcome::NeedsInfo,
        );
        // The first launch has no title, so the run has none, whatever a later launch carries.
        finished_run(
            &store,
            &softfac(3),
            &[
                launched(None, None),
                launched(Some("Later title"), None),
                moved(State::Ready, State::Building),
            ],
            Outcome::PrOpen,
        );
        // No title anywhere: none. A stuck line that the run has since left says nothing.
        finished_run(
            &store,
            &softfac(4),
            &[
                launched(None, None),
                stuck("old"),
                moved(State::Stuck, State::Building),
            ],
            Outcome::PrOpen,
        );
        // Stuck with no stuck line: no reason.
        finished_run(
            &store,
            &softfac(5),
            &[moved(State::Building, State::Stuck)],
            Outcome::Stuck,
        );
        assert_eq!(
            title_and_reason(&store),
            [
                (1, Some("Old title".into()), Some("gate failed".into())),
                (2, None, Some("which db?".into())),
                (3, None, None),
                (4, None, None),
                (5, None, None),
            ]
        );
    }

    #[test]
    fn reads_the_title_and_reason_of_a_run_that_is_not_finished() {
        let (_dir, store) = open_store();
        let ledger = at(&store, T0);
        ledger.write(&softfac(1), &claimed(), &test_cause()).unwrap();
        ledger
            .write(&softfac(1), &launched(Some("Open run"), None), &test_cause())
            .unwrap();
        ledger
            .write(&softfac(1), &stuck("gate failed"), &test_cause())
            .unwrap();
        ledger
            .write(
                &softfac(1),
                &moved(State::Building, State::Stuck),
                &test_cause(),
            )
            .unwrap();
        assert_eq!(
            title_and_reason(&store),
            [(1, Some("Open run".into()), Some("gate failed".into()))]
        );
    }

    #[test]
    fn caps_a_long_reason_at_300_characters_on_a_character_boundary_in_both_reads() {
        let (_dir, store) = open_store();
        let long = "\u{1F600}".repeat(400);
        finished_run(
            &store,
            &softfac(1),
            &[stuck(&long), moved(State::Building, State::Stuck)],
            Outcome::Stuck,
        );
        finished_run(
            &store,
            &softfac(2),
            &[asked("short?"), moved(State::Triage, State::NeedsInfo)],
            Outcome::NeedsInfo,
        );
        let ledger = at(&store, T0);
        ledger.write(&softfac(3), &claimed(), &test_cause()).unwrap();
        ledger.write(&softfac(3), &stuck(&long), &test_cause()).unwrap();
        ledger
            .write(
                &softfac(3),
                &moved(State::Building, State::Stuck),
                &test_cause(),
            )
            .unwrap();
        let reasons: Vec<String> = summaries_of(&store, "rawphp/softfac")
            .into_iter()
            .map(|run| run.reason.unwrap_or_default())
            .collect();
        let capped = format!("{}\u{2026}", "\u{1F600}".repeat(299));
        for reason in [&reasons[0], &reasons[2]] {
            assert_eq!(reason.chars().count(), REASON_MAX_CHARS);
            assert_eq!(*reason, capped);
        }
        assert_eq!(reasons[1], "short?");
    }

    #[test]
    fn leaves_a_reason_of_exactly_300_characters_whole_and_cuts_one_of_301_to_299_and_an_ellipsis()
     {
        let (_dir, store) = open_store();
        let exact = "\u{1F600}".repeat(300);
        finished_run(
            &store,
            &softfac(1),
            &[stuck(&exact), moved(State::Building, State::Stuck)],
            Outcome::Stuck,
        );
        finished_run(
            &store,
            &softfac(2),
            &[
                stuck(&format!("{exact}\u{1F600}")),
                moved(State::Building, State::Stuck),
            ],
            Outcome::Stuck,
        );
        let reasons: Vec<Option<String>> = summaries_of(&store, "rawphp/softfac")
            .into_iter()
            .map(|run| run.reason)
            .collect();
        assert_eq!(
            reasons,
            [
                Some(exact),
                Some(format!("{}\u{2026}", "\u{1F600}".repeat(299)))
            ]
        );
    }

    // The runner's `runId.test.ts`: the run id itself is `foundry_proto::factory::RunId`, whose
    // tests are its own. The next attempt is read from the store.

    #[test]
    fn next_attempt_is_one_when_there_are_no_runs() {
        let (_dir, store) = open_store();
        assert_eq!(
            next_attempt(&store.lock().unwrap(), "rawphp/softfac", 8).unwrap(),
            1
        );
    }

    #[test]
    fn next_attempt_is_one_past_the_highest_for_that_repo_and_issue() {
        let (_dir, store) = open_store();
        for id in [
            rid(8, "rawphp/softfac", 1),
            rid(8, "rawphp/softfac", 3),
            rid(9, "rawphp/softfac", 7),
            rid(8, "rawphp/other", 9),
        ] {
            run_at(&store, &id, T0, None, false, true);
        }
        let store = store.lock().unwrap();
        assert_eq!(next_attempt(&store, "rawphp/softfac", 8).unwrap(), 4);
        assert_eq!(next_attempt(&store, "rawphp/softfac", 9).unwrap(), 8);
        assert_eq!(next_attempt(&store, "rawphp/softfac", 10).unwrap(), 1);
        assert_eq!(next_attempt(&store, "rawphp/other", 8).unwrap(), 10);
    }

    #[test]
    fn next_attempt_matches_a_linear_identifier_in_any_case_and_never_passes_the_largest_attempt()
    {
        // Foundry's issues are GitHub numbers: the repo is what is matched in any case.
        let (_dir, store) = open_store();
        run_at(&store, &rid(8, "rawphp/softfac", u32::MAX), T0, None, false, true);
        assert_eq!(
            next_attempt(&store.lock().unwrap(), "RawPHP/SoftFac", 8).unwrap(),
            u32::MAX
        );
    }
}

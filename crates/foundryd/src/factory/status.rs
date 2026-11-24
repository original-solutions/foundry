//! `factory_status`: writing it, and choosing which one to believe.

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::time::Duration;

    use foundry_proto::factory::{
        FactoryStatus, FinishedRun, ForeignClaim, HeldUp, HeldUpKind, LiveRun, NotHolding, RunId,
        RunOutcome, Waiting,
    };
    use foundry_proto::ids::{MachineId, ProjectId};

    use super::{FINISHED_KEPT, Freshness, PassResults, believed, build, not_holding};
    use crate::clock::{Clock, FakeClock};

    const MINUTE: Duration = Duration::from_secs(60);

    fn holding(project: ProjectId, generation: u64, pass: u64) -> FactoryStatus {
        build(PassResults {
            generation,
            pass,
            ..PassResults::new(project)
        })
    }

    fn no_token(project: ProjectId) -> FactoryStatus {
        not_holding(
            project,
            NotHolding::NoToken {
                login: "octocat".into(),
            },
        )
    }

    fn held_by(project: ProjectId, machine: MachineId, generation: u64) -> FactoryStatus {
        not_holding(
            project,
            NotHolding::HeldBy {
                machine,
                generation,
            },
        )
    }

    fn online(machines: &[MachineId]) -> BTreeSet<MachineId> {
        machines.iter().copied().collect()
    }

    fn run(issue: u64) -> RunId {
        RunId::new("octo/repo", issue, 1).unwrap()
    }

    fn finished(issue: u64) -> FinishedRun {
        FinishedRun {
            run: run(issue),
            title: format!("issue {issue}"),
            outcome: RunOutcome::PrOpen,
            pr_end: None,
            pr_url: None,
        }
    }

    fn freshness() -> (Arc<FakeClock>, Freshness) {
        let clock = Arc::new(FakeClock::new());
        let dyn_clock: Arc<dyn Clock> = clock.clone();
        (clock, Freshness::new(dyn_clock))
    }

    // believed

    #[test]
    fn a_dead_holders_status_at_generation_5_is_not_believed_over_the_acting_coordinators_no_token()
    {
        let project = ProjectId::random();
        let (dead, acting) = (MachineId::random(), MachineId::random());
        let statuses = vec![
            (dead, holding(project, 5, 40)),
            (acting, no_token(project)),
        ];

        let believed = believed(&statuses, &online(&[acting]), acting);

        assert_eq!(believed, Some(no_token(project)));
    }

    #[test]
    fn a_holding_status_beats_a_not_holding_one_at_the_same_generation() {
        let project = ProjectId::random();
        let (holder, acting) = (MachineId::random(), MachineId::random());
        let holds = holding(project, 5, 3);
        let statuses = vec![
            (acting, held_by(project, holder, 5)),
            (holder, holds.clone()),
        ];

        let believed = believed(&statuses, &online(&[holder, acting]), acting);

        assert_eq!(believed, Some(holds));
    }

    #[test]
    fn a_holding_status_beats_a_not_holding_one_at_a_higher_generation() {
        let project = ProjectId::random();
        let (holder, other, acting) = (MachineId::random(), MachineId::random(), MachineId::random());
        let holds = holding(project, 4, 3);
        let statuses = vec![
            (acting, held_by(project, other, 6)),
            (holder, holds.clone()),
        ];

        let believed = believed(&statuses, &online(&[holder, acting]), acting);

        assert_eq!(believed, Some(holds));
    }

    #[test]
    fn of_two_online_holders_the_one_at_the_highest_generation_is_believed() {
        let project = ProjectId::random();
        let (old, new, acting) = (MachineId::random(), MachineId::random(), MachineId::random());
        let newer = holding(project, 7, 1);
        let statuses = vec![
            (old, holding(project, 6, 900)),
            (new, newer.clone()),
            (acting, held_by(project, new, 7)),
        ];

        for order in [statuses.clone(), statuses.into_iter().rev().collect()] {
            let believed = believed(&order, &online(&[old, new, acting]), acting);
            assert_eq!(believed, Some(newer.clone()));
        }
    }

    #[test]
    fn with_no_holder_online_the_acting_coordinators_status_is_believed() {
        let project = ProjectId::random();
        let (other, acting) = (MachineId::random(), MachineId::random());
        let holder = MachineId::random();
        let acting_says = held_by(project, holder, 3);
        let statuses = vec![
            (other, no_token(project)),
            (acting, acting_says.clone()),
        ];

        let believed = believed(&statuses, &online(&[other, acting]), acting);

        assert_eq!(believed, Some(acting_says));
    }

    #[test]
    fn a_status_from_a_writer_that_is_not_online_is_never_believed() {
        let project = ProjectId::random();
        let (gone, acting) = (MachineId::random(), MachineId::random());
        let statuses = vec![(acting, no_token(project)), (gone, no_token(project))];

        assert_eq!(believed(&statuses, &online(&[gone]), acting), None);
        assert_eq!(believed(&statuses, &online(&[]), acting), None);
    }

    #[test]
    fn nothing_is_believed_when_no_holder_is_online_and_the_acting_coordinator_wrote_none() {
        let project = ProjectId::random();
        let (other, acting) = (MachineId::random(), MachineId::random());
        let statuses = vec![(other, no_token(project))];

        assert_eq!(believed(&statuses, &online(&[other, acting]), acting), None);
        assert_eq!(believed(&[], &online(&[acting]), acting), None);
    }

    // Freshness

    #[test]
    fn a_not_holding_status_never_resets_since_holding() {
        let project = ProjectId::random();
        let holder = MachineId::random();
        let (clock, freshness) = freshness();
        freshness.received(&holding(project, 5, 1));
        clock.advance(9 * MINUTE);

        for status in [
            no_token(project),
            held_by(project, holder, 5),
            held_by(project, holder, 6),
            FactoryStatus {
                pass: 99,
                generation: 7,
                ..no_token(project)
            },
        ] {
            freshness.received(&status);
            assert_eq!(freshness.since_holding(project), 9 * MINUTE);
        }

        clock.advance(MINUTE);
        assert_eq!(freshness.since_holding(project), 10 * MINUTE);
    }

    #[test]
    fn holding_the_lease_resets_since_holding() {
        let project = ProjectId::random();
        let (clock, freshness) = freshness();
        clock.advance(12 * MINUTE);

        freshness.held(project);
        assert_eq!(freshness.since_holding(project), Duration::ZERO);

        clock.advance(MINUTE);
        assert_eq!(freshness.since_holding(project), MINUTE);
        freshness.held(project);
        assert_eq!(freshness.since_holding(project), Duration::ZERO);
    }

    #[test]
    fn a_project_never_held_nor_heard_counts_from_when_freshness_was_made() {
        let project = ProjectId::random();
        let clock = Arc::new(FakeClock::new());
        clock.advance(30 * MINUTE);
        let freshness = Freshness::new(clock.clone());

        assert_eq!(freshness.since_holding(project), Duration::ZERO);
        clock.advance(4 * MINUTE);
        assert_eq!(freshness.since_holding(project), 4 * MINUTE);
    }

    #[test]
    fn a_holding_status_with_a_higher_pass_resets_since_holding() {
        let project = ProjectId::random();
        let (clock, freshness) = freshness();
        freshness.received(&holding(project, 5, 10));
        clock.advance(2 * MINUTE);

        freshness.received(&holding(project, 5, 11));

        assert_eq!(freshness.since_holding(project), Duration::ZERO);
    }

    #[test]
    fn the_same_holding_status_received_again_does_not_reset_since_holding() {
        let project = ProjectId::random();
        let (clock, freshness) = freshness();
        freshness.received(&holding(project, 5, 10));
        clock.advance(2 * MINUTE);

        freshness.received(&holding(project, 5, 10));
        freshness.received(&holding(project, 5, 9));

        assert_eq!(freshness.since_holding(project), 2 * MINUTE);
    }

    #[test]
    fn a_holding_status_below_the_highest_generation_seen_does_not_reset_since_holding() {
        let project = ProjectId::random();
        let (clock, freshness) = freshness();
        freshness.received(&holding(project, 6, 1));
        clock.advance(3 * MINUTE);

        freshness.received(&holding(project, 5, 500));

        assert_eq!(freshness.since_holding(project), 3 * MINUTE);
    }

    #[test]
    fn a_not_holding_status_raises_the_generation_a_holding_status_must_reach() {
        let project = ProjectId::random();
        let new = MachineId::random();
        let (clock, freshness) = freshness();
        freshness.received(&holding(project, 5, 1));
        freshness.received(&held_by(project, new, 6));
        clock.advance(3 * MINUTE);

        // The old holder, at generation 5, has been taken over.
        freshness.received(&holding(project, 5, 2));
        assert_eq!(freshness.since_holding(project), 3 * MINUTE);

        freshness.received(&holding(project, 6, 1));
        assert_eq!(freshness.since_holding(project), Duration::ZERO);
    }

    #[test]
    fn a_holding_status_at_a_new_generation_resets_since_holding_whatever_its_pass() {
        let project = ProjectId::random();
        let (clock, freshness) = freshness();
        freshness.received(&holding(project, 5, 300));
        clock.advance(4 * MINUTE);

        freshness.received(&holding(project, 6, 1));

        assert_eq!(freshness.since_holding(project), Duration::ZERO);
    }

    #[test]
    fn each_project_keeps_its_own_freshness() {
        let (fresh, stale) = (ProjectId::random(), ProjectId::random());
        let (clock, freshness) = freshness();
        freshness.received(&holding(stale, 2, 1));
        clock.advance(11 * MINUTE);

        freshness.held(fresh);
        freshness.received(&holding(fresh, 9, 9));

        assert_eq!(freshness.since_holding(fresh), Duration::ZERO);
        assert_eq!(freshness.since_holding(stale), 11 * MINUTE);
    }

    #[test]
    fn a_clock_that_jumps_is_counted_in_full() {
        let project = ProjectId::random();
        let (clock, freshness) = freshness();
        freshness.held(project);

        clock.advance(20 * MINUTE);

        assert_eq!(freshness.since_holding(project), 20 * MINUTE);
    }

    // build and not_holding

    #[test]
    fn a_built_status_holds_the_lease_and_carries_the_pass_results() {
        let project = ProjectId::random();
        let machine = MachineId::random();
        let ready = vec![Waiting {
            issue: 4,
            title: "four".into(),
            reason: "no room".into(),
            run: None,
        }];
        let feedback_waiting = vec![Waiting {
            issue: 5,
            title: "five".into(),
            reason: "no room".into(),
            run: Some(run(5)),
        }];
        let runs = vec![LiveRun {
            run: run(6),
            title: "six".into(),
            step: "building".into(),
            machine,
            gate_retries_used: 1,
            minutes_used: 12,
            workspaces: vec![],
            sessions: vec![],
        }];
        let held_up = vec![HeldUp {
            run: run(7),
            kind: HeldUpKind::Stuck,
            reason: "gate still red".into(),
        }];
        let foreign = vec![ForeignClaim {
            issue: 8,
            title: "eight".into(),
            state: "building".into(),
        }];

        let status = build(PassResults {
            project,
            pass: 12,
            generation: 3,
            idle: Some("gh failed three passes running".into()),
            owner_ignored: true,
            ready: ready.clone(),
            feedback_waiting: feedback_waiting.clone(),
            runs: runs.clone(),
            held_up: held_up.clone(),
            gate_infra: vec![run(9)],
            finished: vec![finished(10)],
            foreign: foreign.clone(),
        });

        assert_eq!(
            status,
            FactoryStatus {
                project,
                pass: 12,
                holds: true,
                generation: 3,
                not_holding: None,
                idle: Some("gh failed three passes running".into()),
                owner_ignored: true,
                ready,
                feedback_waiting,
                runs,
                held_up,
                gate_infra: vec![run(9)],
                finished: vec![finished(10)],
                foreign,
            }
        );
    }

    #[test]
    fn a_built_status_keeps_the_newest_20_finished_runs() {
        let project = ProjectId::random();
        let newest_first: Vec<_> = (0..25).rev().map(finished).collect();

        let status = build(PassResults {
            finished: newest_first.clone(),
            ..PassResults::new(project)
        });

        assert_eq!(FINISHED_KEPT, 20);
        assert_eq!(status.finished, newest_first[..20].to_vec());
    }

    #[test]
    fn empty_pass_results_build_a_holding_status_with_nothing_in_it() {
        let project = ProjectId::random();

        let status = build(PassResults::new(project));

        assert!(status.holds);
        assert_eq!(status.project, project);
        assert_eq!((status.pass, status.generation), (0, 0));
        assert_eq!(status.not_holding, None);
        assert_eq!(status.idle, None);
        assert!(!status.owner_ignored);
        assert!(status.ready.is_empty() && status.runs.is_empty() && status.finished.is_empty());
    }

    #[test]
    fn a_not_holding_status_says_why_and_nothing_more() {
        let project = ProjectId::random();
        let holder = MachineId::random();

        let status = held_by(project, holder, 8);

        assert_eq!(
            status,
            FactoryStatus {
                project,
                pass: 0,
                holds: false,
                generation: 8,
                not_holding: Some(NotHolding::HeldBy {
                    machine: holder,
                    generation: 8,
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
            }
        );
    }

    #[test]
    fn a_no_token_status_names_the_login_and_no_generation() {
        let project = ProjectId::random();

        let status = no_token(project);

        assert!(!status.holds);
        assert_eq!(status.generation, 0);
        assert_eq!(
            status.not_holding,
            Some(NotHolding::NoToken {
                login: "octocat".into()
            })
        );
    }
}

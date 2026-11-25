//! Which machine takes a run or a gate.

use std::collections::BTreeMap;

use foundry_proto::factory::FactorySettings;
use foundry_proto::ids::MachineId;
use foundry_proto::settings::SharedSettings;

use crate::factory::ledger::list::RunInfo;
use crate::factory::worker::FleetView;

/// The machines picked for runs so far in one pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reserved {
    picked: BTreeMap<MachineId, u32>,
}

/// The run and gate slots free.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slots {
    pub runs_free: u32,
    pub gates_free: u32,
}

/// The machine a run's session goes to.
pub fn place_run(
    _view: &FleetView,
    _settings: &SharedSettings,
    _login: &str,
    _reserved: &mut Reserved,
) -> Result<MachineId, Vec<String>> {
    todo!()
}

/// The machine a gate goes to.
pub fn place_gate(
    _view: &FleetView,
    _settings: &SharedSettings,
    _login: &str,
) -> Result<MachineId, Vec<String>> {
    todo!()
}

/// The slots free.
pub fn slots(_runs: &[RunInfo], _gates: usize, _settings: &FactorySettings) -> Slots {
    todo!()
}

#[cfg(test)]
mod tests {
    use foundry_proto::factory::{MachineHeld, RunId};
    use foundry_proto::record::{MachineSample, Record};
    use foundry_proto::settings::{Limits, Machine};

    use super::*;
    use crate::factory::lifecycle::state::State;

    const LOGIN: &str = "rawphp";

    fn sample(owner: MachineId, sessions: u32, load_per_core: f64) -> Record<MachineSample> {
        Record {
            owner,
            version: 3,
            deleted: false,
            body: MachineSample {
                load_per_core,
                free_memory: 8_000_000_000,
                free_disk: 100_000_000_000,
                sessions,
                update_refused: None,
            },
        }
    }

    fn holds(logins: &[&str], skill: bool) -> MachineHeld {
        MachineHeld {
            logins: logins.iter().map(|l| (*l).to_owned()).collect(),
            skill,
        }
    }

    /// Settings naming each machine, each with a limit of 4 sessions, load 1.0 and 10 GB free.
    fn settings(named: &[(MachineId, &str)]) -> SharedSettings {
        SharedSettings {
            machines: named
                .iter()
                .map(|(id, name)| Machine {
                    id: *id,
                    name: (*name).to_owned(),
                    address: String::new(),
                    key: String::new(),
                })
                .collect(),
            limits: named
                .iter()
                .map(|(id, _)| {
                    (
                        *id,
                        Limits {
                            max_sessions: 4,
                            max_load_per_core: 1.0,
                            min_free_disk: 10_000_000_000,
                        },
                    )
                })
                .collect(),
            ..SharedSettings::default()
        }
    }

    fn two() -> (MachineId, MachineId, SharedSettings) {
        let [mac, vps] = [MachineId::random(), MachineId::random()];
        (mac, vps, settings(&[(mac, "mac"), (vps, "vps")]))
    }

    #[test]
    fn a_machine_with_no_token_for_the_login_is_never_picked() {
        let (mac, vps, settings) = two();
        // The mac is the roomier, but holds a token only for another login.
        let view = FleetView::builder()
            .sample(sample(mac, 0, 0.1))
            .sample(sample(vps, 2, 0.8))
            .held(mac, holds(&["someone-else"], true))
            .held(vps, holds(&[LOGIN], true))
            .online(mac)
            .online(vps)
            .build();
        let mut reserved = Reserved::default();
        assert_eq!(place_run(&view, &settings, LOGIN, &mut reserved), Ok(vps));
        assert_eq!(place_gate(&view, &settings, LOGIN), Ok(vps));
    }

    #[test]
    fn the_reason_a_machine_with_no_token_is_passed_over_names_the_login() {
        let (mac, _, settings) = two();
        let view = FleetView::builder()
            .sample(sample(mac, 0, 0.1))
            .held(mac, holds(&["someone-else"], true))
            .online(mac)
            .build();
        let reasons = vec!["mac: no token for rawphp".to_owned()];
        let mut reserved = Reserved::default();
        assert_eq!(
            place_run(&view, &settings, LOGIN, &mut reserved),
            Err(reasons.clone())
        );
        assert_eq!(place_gate(&view, &settings, LOGIN), Err(reasons));
    }

    #[test]
    fn a_login_is_matched_in_its_exact_case_as_the_token_is_looked_up() {
        let (mac, _, settings) = two();
        let view = FleetView::builder()
            .sample(sample(mac, 0, 0.1))
            .held(mac, holds(&["RawPHP"], true))
            .online(mac)
            .build();
        let mut reserved = Reserved::default();
        assert_eq!(
            place_run(&view, &settings, LOGIN, &mut reserved),
            Err(vec!["mac: no token for rawphp".to_owned()])
        );
    }

    #[test]
    fn a_machine_with_no_machine_held_is_never_picked() {
        let (mac, vps, settings) = two();
        // The mac is on protocol 4: it has published no `MachineHeld`, so says nothing of what
        // it holds, though it is the roomier.
        let view = FleetView::builder()
            .sample(sample(mac, 0, 0.1))
            .sample(sample(vps, 2, 0.8))
            .held(vps, holds(&[LOGIN], true))
            .online(mac)
            .online(vps)
            .build();
        let mut reserved = Reserved::default();
        assert_eq!(place_run(&view, &settings, LOGIN, &mut reserved), Ok(vps));
        assert_eq!(place_gate(&view, &settings, LOGIN), Ok(vps));

        let alone = FleetView::builder()
            .sample(sample(mac, 0, 0.1))
            .online(mac)
            .build();
        let reasons = vec!["mac: has said nothing of the logins it holds (protocol 4)".to_owned()];
        assert_eq!(
            place_run(&alone, &settings, LOGIN, &mut reserved),
            Err(reasons.clone())
        );
        assert_eq!(place_gate(&alone, &settings, LOGIN), Err(reasons));
    }

    #[test]
    fn a_machine_without_the_skill_is_never_picked() {
        let (mac, vps, settings) = two();
        let view = FleetView::builder()
            .sample(sample(mac, 0, 0.1))
            .sample(sample(vps, 2, 0.8))
            .held(mac, holds(&[LOGIN], false))
            .held(vps, holds(&[LOGIN], true))
            .online(mac)
            .online(vps)
            .build();
        let mut reserved = Reserved::default();
        assert_eq!(place_run(&view, &settings, LOGIN, &mut reserved), Ok(vps));
        assert_eq!(place_gate(&view, &settings, LOGIN), Ok(vps));
    }

    #[test]
    fn every_reason_a_machine_is_passed_over_is_given_in_one() {
        let (mac, _, settings) = two();
        let view = FleetView::builder()
            .sample(sample(mac, 0, 0.1))
            .held(mac, holds(&[], false))
            .online(mac)
            .build();
        let mut reserved = Reserved::default();
        assert_eq!(
            place_run(&view, &settings, LOGIN, &mut reserved),
            Err(vec![
                "mac: no token for rawphp, no factory-build skill".to_owned()
            ])
        );
    }

    #[test]
    fn an_offline_machine_is_never_picked() {
        let (mac, vps, settings) = two();
        let view = FleetView::builder()
            .sample(sample(mac, 0, 0.1))
            .sample(sample(vps, 2, 0.8))
            .held(mac, holds(&[LOGIN], true))
            .held(vps, holds(&[LOGIN], true))
            .online(vps)
            .build();
        let mut reserved = Reserved::default();
        assert_eq!(place_run(&view, &settings, LOGIN, &mut reserved), Ok(vps));
        assert_eq!(place_gate(&view, &settings, LOGIN), Ok(vps));

        let alone = FleetView::builder()
            .sample(sample(mac, 0, 0.1))
            .held(mac, holds(&[LOGIN], true))
            .build();
        assert_eq!(
            place_run(&alone, &settings, LOGIN, &mut reserved),
            Err(vec!["mac: offline".to_owned()])
        );
    }

    #[test]
    fn two_claims_in_one_pass_do_not_both_land_on_the_machine_with_one_free_session() {
        let (mac, vps, settings) = two();
        // The mac has the lower load but only one of its 4 sessions free.
        let view = FleetView::builder()
            .sample(sample(mac, 3, 0.1))
            .sample(sample(vps, 0, 0.5))
            .held(mac, holds(&[LOGIN], true))
            .held(vps, holds(&[LOGIN], true))
            .online(mac)
            .online(vps)
            .build();
        let mut reserved = Reserved::default();
        assert_eq!(place_run(&view, &settings, LOGIN, &mut reserved), Ok(mac));
        assert_eq!(place_run(&view, &settings, LOGIN, &mut reserved), Ok(vps));
    }

    #[test]
    fn a_second_claim_is_refused_when_the_one_free_session_was_taken_this_pass() {
        let (mac, _, settings) = two();
        let view = FleetView::builder()
            .sample(sample(mac, 3, 0.1))
            .held(mac, holds(&[LOGIN], true))
            .online(mac)
            .build();
        let mut reserved = Reserved::default();
        assert_eq!(place_run(&view, &settings, LOGIN, &mut reserved), Ok(mac));
        assert_eq!(
            place_run(&view, &settings, LOGIN, &mut reserved),
            Err(vec!["mac: sessions 4 at limit 4".to_owned()])
        );
        // A new pass starts with nothing reserved.
        assert_eq!(
            place_run(&view, &settings, LOGIN, &mut Reserved::default()),
            Ok(mac)
        );
    }

    #[test]
    fn a_refused_claim_reserves_nothing() {
        let (mac, vps, settings) = two();
        let full = FleetView::builder()
            .sample(sample(mac, 4, 0.1))
            .held(mac, holds(&[LOGIN], true))
            .online(mac)
            .build();
        let mut reserved = Reserved::default();
        assert!(place_run(&full, &settings, LOGIN, &mut reserved).is_err());
        assert_eq!(reserved, Reserved::default());

        let view = FleetView::builder()
            .sample(sample(mac, 3, 0.1))
            .sample(sample(vps, 0, 0.5))
            .held(mac, holds(&[LOGIN], true))
            .held(vps, holds(&[LOGIN], true))
            .online(mac)
            .online(vps)
            .build();
        assert_eq!(place_run(&view, &settings, LOGIN, &mut reserved), Ok(mac));
    }

    #[test]
    fn a_gate_is_placed_on_a_machine_at_its_session_limit() {
        let (mac, _, settings) = two();
        let view = FleetView::builder()
            .sample(sample(mac, 4, 0.1))
            .held(mac, holds(&[LOGIN], true))
            .online(mac)
            .build();
        let mut reserved = Reserved::default();
        assert_eq!(
            place_run(&view, &settings, LOGIN, &mut reserved),
            Err(vec!["mac: sessions 4 at limit 4".to_owned()])
        );
        assert_eq!(place_gate(&view, &settings, LOGIN), Ok(mac));
    }

    #[test]
    fn a_gate_still_keeps_to_the_load_and_disk_limits() {
        let (mac, vps, settings) = two();
        let mut low_disk = sample(vps, 5, 0.1);
        low_disk.body.free_disk = 1_000_000_000;
        let view = FleetView::builder()
            .sample(sample(mac, 5, 1.5))
            .sample(low_disk)
            .held(mac, holds(&[LOGIN], true))
            .held(vps, holds(&[LOGIN], true))
            .online(mac)
            .online(vps)
            .build();
        assert_eq!(
            place_gate(&view, &settings, LOGIN),
            Err(vec![
                "mac: load 1.5 over limit 1.0".to_owned(),
                "vps: free disk 1.0 GB under limit 10.0 GB".to_owned(),
            ])
        );
    }

    #[test]
    fn the_newest_sample_of_a_machine_is_the_one_read() {
        let (mac, vps, settings) = two();
        let mut newer = sample(mac, 4, 0.1);
        newer.version = 9;
        let view = FleetView::builder()
            .sample(sample(mac, 0, 0.1))
            .sample(newer)
            .sample(sample(vps, 0, 0.5))
            .held(mac, holds(&[LOGIN], true))
            .held(vps, holds(&[LOGIN], true))
            .online(mac)
            .online(vps)
            .build();
        let mut reserved = Reserved::default();
        assert_eq!(place_run(&view, &settings, LOGIN, &mut reserved), Ok(vps));
    }

    #[test]
    fn a_machine_whose_sample_was_removed_is_not_picked() {
        let (mac, vps, settings) = two();
        let mut removed = sample(mac, 0, 0.1);
        removed.version = 9;
        removed.deleted = true;
        let view = FleetView::builder()
            .sample(sample(mac, 0, 0.1))
            .sample(removed)
            .sample(sample(vps, 0, 0.5))
            .held(mac, holds(&[LOGIN], true))
            .held(vps, holds(&[LOGIN], true))
            .online(mac)
            .online(vps)
            .build();
        let mut reserved = Reserved::default();
        assert_eq!(place_run(&view, &settings, LOGIN, &mut reserved), Ok(vps));
    }

    #[test]
    fn a_machine_the_settings_do_not_name_is_named_by_its_id() {
        let mac = MachineId::random();
        let view = FleetView::builder()
            .sample(sample(mac, 0, 0.1))
            .held(mac, holds(&[], true))
            .online(mac)
            .build();
        let mut reserved = Reserved::default();
        assert_eq!(
            place_run(&view, &SharedSettings::default(), LOGIN, &mut reserved),
            Err(vec![format!("{mac}: no token for rawphp")])
        );
    }

    #[test]
    fn with_no_machine_there_is_no_pick_and_no_reason() {
        let mut reserved = Reserved::default();
        assert_eq!(
            place_run(
                &FleetView::default(),
                &SharedSettings::default(),
                LOGIN,
                &mut reserved
            ),
            Err(vec![])
        );
    }

    fn run(issue: u64, state: Option<State>, confirmed: bool, pr_reported: bool) -> RunInfo {
        RunInfo {
            id: RunId::new("acme/app", issue, 1).expect("a legal repo"),
            state,
            unreadable: false,
            confirmed,
            session: confirmed,
            pr_reported,
            base: None,
        }
    }

    #[test]
    fn a_run_slot_is_held_as_holds_slot_counts() {
        let settings = FactorySettings {
            max_runs: 4,
            ..FactorySettings::default()
        };
        let mut unreadable = run(5, None, false, false);
        unreadable.unreadable = true;
        let runs = [
            // Holds one: building.
            run(1, Some(State::Building), true, false),
            // Holds none: waiting on its pull request.
            run(2, Some(State::PrOpen), true, false),
            // Holds none: reported, waiting for the gate.
            run(3, Some(State::Building), true, true),
            // Holds none: never confirmed.
            run(4, Some(State::Triage), false, false),
            // Holds one: its events cannot be read.
            unreadable,
        ];
        assert_eq!(slots(&runs, 0, &settings).runs_free, 2);
    }

    #[test]
    fn a_gate_slot_is_held_by_each_gate_running() {
        let settings = FactorySettings {
            max_gates: 2,
            ..FactorySettings::default()
        };
        assert_eq!(
            slots(&[], 0, &settings),
            Slots {
                runs_free: 3,
                gates_free: 2
            }
        );
        assert_eq!(slots(&[], 1, &settings).gates_free, 1);
    }

    #[test]
    fn more_held_than_the_limit_leaves_none_free() {
        let settings = FactorySettings {
            max_runs: 1,
            max_gates: 1,
            ..FactorySettings::default()
        };
        let runs = [
            run(1, Some(State::Building), true, false),
            run(2, Some(State::Triage), true, false),
        ];
        assert_eq!(
            slots(&runs, 3, &settings),
            Slots {
                runs_free: 0,
                gates_free: 0
            }
        );
    }
}

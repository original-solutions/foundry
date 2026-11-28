//! Which machine takes a run or a gate.
//!
//! Placement is for the fleet: it is not asked for a project built on a host, so it answers a
//! [`Target::Machine`] and never a host.
//!
//! A machine qualifies for the factory only when it is online, has published a
//! [`MachineHeld`] (a machine on protocol 4 never does, so the factory places nothing there),
//! holds a token for the project's login and has the `factory-build` skill. Among those, P1's
//! [`placement::place`] picks: the lowest load per core under every limit, then the fewest
//! sessions.
//!
//! [`place_run`] keeps to every limit, the session limit included, and counts each machine it
//! picked earlier in the same pass one session fuller, as the spec's shared settings say of a
//! pass that claims more than one issue, and so too each run the caller holds on a machine with
//! [`Reserved::hold`]. [`place_gate`] keeps to the load and disk limits only:
//! a gate has no session. Neither sends anything; the caller makes the workspace.
//!
//! [`slots`] counts what is free of the fleet's `max_runs` and `max_gates`.

use std::collections::BTreeMap;

use foundry_proto::factory::{FactorySettings, MachineHeld};
use foundry_proto::ids::MachineId;
use foundry_proto::record::{MachineSample, Record};
use foundry_proto::settings::{Limits, SharedSettings};

use crate::factory::ledger::list::{RunInfo, holds_slot};
use crate::factory::worker::{FleetView, Target};
use crate::placement;

/// The runs placed or held on each machine in one pass, each counting its machine one session
/// fuller for the rest of it: those [`place_run`] picked, and those the caller held with
/// [`Reserved::hold`]. A new pass starts with a new one.
///
/// A machine's samples count a run's session only once it has started, after its workspace's
/// setup, which can take minutes and so outlast the pass that placed it. The caller tells the
/// pass about such runs with [`Reserved::hold`] before it places anything, so a run placed by
/// an earlier pass, or by another project's, still keeps the machine's limit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reserved {
    held: BTreeMap<MachineId, u32>,
}

impl Reserved {
    /// Counts `machine` one session fuller for every later [`place_run`]: a run is placed there
    /// whose session its samples do not count yet.
    pub fn hold(&mut self, machine: MachineId) {
        *self.held.entry(machine).or_insert(0) += 1;
    }

    /// How many runs are placed or held on `machine`.
    fn on(&self, machine: MachineId) -> u32 {
        self.held.get(&machine).copied().unwrap_or(0)
    }
}

/// What is free of the fleet's run and gate slots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slots {
    /// Of `max_runs`.
    pub runs_free: u32,
    /// Of `max_gates`.
    pub gates_free: u32,
}

/// The machine a run's session goes to, among the machines that qualify (see the module), or
/// one reason per machine passed over, in the order of the view's samples, such as
/// `vps: no token for rawphp` or `mac: sessions 4 at limit 4`. Each machine named by
/// `settings`, or by its id when they do not name it. The machine picked is counted one session
/// fuller in `reserved` for the rest of the pass; a refusal reserves nothing. The answer is
/// always a [`Target::Machine`].
pub fn place_run(
    view: &FleetView,
    settings: &SharedSettings,
    login: &str,
    reserved: &mut Reserved,
) -> Result<Target, Vec<String>> {
    let machine = pick(view, settings, login, reserved, &settings.limits)?;
    reserved.hold(machine);
    Ok(Target::Machine(machine))
}

/// The machine a gate goes to: as [`place_run`], but with no session limit, since a gate has
/// no session. The load and disk limits still hold. Reserves nothing. The answer is always a
/// [`Target::Machine`].
pub fn place_gate(
    view: &FleetView,
    settings: &SharedSettings,
    login: &str,
) -> Result<Target, Vec<String>> {
    let limits = settings
        .limits
        .iter()
        .map(|(machine, limits)| {
            let mut limits = limits.clone();
            limits.max_sessions = u32::MAX;
            (*machine, limits)
        })
        .collect();
    pick(view, settings, login, &Reserved::default(), &limits).map(Target::Machine)
}

/// The run and gate slots free: `max_runs` less the `runs` that hold a slot as
/// [`holds_slot`] counts them, and `max_gates` less the `gates` running. Never below zero.
pub fn slots(runs: &[RunInfo], gates: usize, settings: &FactorySettings) -> Slots {
    let held = runs.iter().filter(|run| holds_slot(run)).count();
    Slots {
        runs_free: settings.max_runs.saturating_sub(saturated(held)),
        gates_free: settings.max_gates.saturating_sub(saturated(gates)),
    }
}

fn saturated(count: usize) -> u32 {
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// The pick of [`placement::place`] among the machines that qualify, under `limits`, with the
/// sessions `reserved` holds added to each machine's newest sample.
fn pick(
    view: &FleetView,
    settings: &SharedSettings,
    login: &str,
    reserved: &Reserved,
    limits: &BTreeMap<MachineId, Limits>,
) -> Result<MachineId, Vec<String>> {
    let samples = newest(view.samples());
    let mut refused: BTreeMap<MachineId, String> = BTreeMap::new();
    let mut candidates = Vec::new();
    for sample in &samples {
        let machine = sample.owner;
        // An offline machine is left to `place`, which says so: what it last said it holds may
        // be out of date.
        let why = if view.online(machine) {
            not_held(view.held(machine), login)
        } else {
            None
        };
        match why {
            Some(why) => {
                refused.insert(machine, format!("{}: {why}", name(settings, machine)));
            }
            None => {
                let mut sample = sample.clone();
                sample.body.sessions = sample.body.sessions.saturating_add(reserved.on(machine));
                candidates.push(sample);
            }
        }
    }
    let online = candidates
        .iter()
        .map(|s| s.owner)
        .filter(|machine| view.online(*machine))
        .collect();
    let reasons = match placement::place(&candidates, limits, &online) {
        Ok(machine) => return Ok(machine),
        Err(reasons) => reasons,
    };
    for reason in reasons {
        refused.insert(reason.machine, reason.text(&name(settings, reason.machine)));
    }
    Err(samples
        .iter()
        .filter_map(|s| refused.remove(&s.owner))
        .collect())
}

/// Why a machine that holds `held` cannot take a run of `login`, or `None` when it can. The
/// login is matched in its exact case, as the machine's secret store looks its token up.
fn not_held(held: Option<&MachineHeld>, login: &str) -> Option<String> {
    let Some(held) = held else {
        return Some("has said nothing of the logins it holds (protocol 4)".to_owned());
    };
    let mut why = Vec::new();
    if !held.logins.iter().any(|held| held == login) {
        why.push(format!("no token for {login}"));
    }
    if !held.skill {
        why.push("no factory-build skill".to_owned());
    }
    (!why.is_empty()).then(|| why.join(", "))
}

/// Each machine's newest sample, in the order each machine first appears in `samples`. A
/// machine whose newest sample is removed is left out.
fn newest(samples: &[Record<MachineSample>]) -> Vec<Record<MachineSample>> {
    let mut order: Vec<MachineId> = Vec::new();
    let mut newest: BTreeMap<MachineId, &Record<MachineSample>> = BTreeMap::new();
    for sample in samples {
        match newest.get(&sample.owner) {
            None => {
                order.push(sample.owner);
                newest.insert(sample.owner, sample);
            }
            Some(held) if sample.version > held.version => {
                newest.insert(sample.owner, sample);
            }
            Some(_) => {}
        }
    }
    order
        .into_iter()
        .filter_map(|machine| newest.get(&machine).copied())
        .filter(|sample| !sample.deleted)
        .cloned()
        .collect()
}

/// `machine`'s name in `settings`, or its id when they do not name it.
fn name(settings: &SharedSettings, machine: MachineId) -> String {
    settings
        .machines
        .iter()
        .find(|m| m.id == machine)
        .map_or_else(|| machine.to_string(), |m| m.name.clone())
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
                version: None,
                update_ready: None,
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
        assert_eq!(
            place_run(&view, &settings, LOGIN, &mut reserved),
            Ok(Target::Machine(vps))
        );
        assert_eq!(
            place_gate(&view, &settings, LOGIN),
            Ok(Target::Machine(vps))
        );
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
        assert_eq!(
            place_run(&view, &settings, LOGIN, &mut reserved),
            Ok(Target::Machine(vps))
        );
        assert_eq!(
            place_gate(&view, &settings, LOGIN),
            Ok(Target::Machine(vps))
        );

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
        assert_eq!(
            place_run(&view, &settings, LOGIN, &mut reserved),
            Ok(Target::Machine(vps))
        );
        assert_eq!(
            place_gate(&view, &settings, LOGIN),
            Ok(Target::Machine(vps))
        );
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
        assert_eq!(
            place_run(&view, &settings, LOGIN, &mut reserved),
            Ok(Target::Machine(vps))
        );
        assert_eq!(
            place_gate(&view, &settings, LOGIN),
            Ok(Target::Machine(vps))
        );

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
        assert_eq!(
            place_run(&view, &settings, LOGIN, &mut reserved),
            Ok(Target::Machine(mac))
        );
        assert_eq!(
            place_run(&view, &settings, LOGIN, &mut reserved),
            Ok(Target::Machine(vps))
        );
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
        assert_eq!(
            place_run(&view, &settings, LOGIN, &mut reserved),
            Ok(Target::Machine(mac))
        );
        assert_eq!(
            place_run(&view, &settings, LOGIN, &mut reserved),
            Err(vec!["mac: sessions 4 at limit 4".to_owned()])
        );
        // A new pass starts with nothing reserved.
        assert_eq!(
            place_run(&view, &settings, LOGIN, &mut Reserved::default()),
            Ok(Target::Machine(mac))
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
        assert_eq!(
            place_run(&view, &settings, LOGIN, &mut reserved),
            Ok(Target::Machine(mac))
        );
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
        assert_eq!(
            place_gate(&view, &settings, LOGIN),
            Ok(Target::Machine(mac))
        );
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
        assert_eq!(
            place_run(&view, &settings, LOGIN, &mut reserved),
            Ok(Target::Machine(vps))
        );

        // The same samples with the newer one held first: the version decides, not the order.
        let mut newer = sample(mac, 4, 0.1);
        newer.version = 9;
        let view = FleetView::builder()
            .sample(newer)
            .sample(sample(mac, 0, 0.1))
            .sample(sample(vps, 0, 0.5))
            .held(mac, holds(&[LOGIN], true))
            .held(vps, holds(&[LOGIN], true))
            .online(mac)
            .online(vps)
            .build();
        let mut reserved = Reserved::default();
        assert_eq!(
            place_run(&view, &settings, LOGIN, &mut reserved),
            Ok(Target::Machine(vps))
        );
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
        assert_eq!(
            place_run(&view, &settings, LOGIN, &mut reserved),
            Ok(Target::Machine(vps))
        );
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

    #[test]
    fn a_machine_that_holds_the_login_among_others_is_picked() {
        let (mac, _, settings) = two();
        let view = FleetView::builder()
            .sample(sample(mac, 0, 0.1))
            .held(mac, holds(&["someone-else", LOGIN], true))
            .online(mac)
            .build();
        let mut reserved = Reserved::default();
        assert_eq!(
            place_run(&view, &settings, LOGIN, &mut reserved),
            Ok(Target::Machine(mac))
        );
        assert_eq!(
            place_gate(&view, &settings, LOGIN),
            Ok(Target::Machine(mac))
        );
    }

    #[test]
    fn a_login_that_only_begins_with_the_projects_is_no_token_for_it() {
        let (mac, _, settings) = two();
        let view = FleetView::builder()
            .sample(sample(mac, 0, 0.1))
            .held(mac, holds(&["rawphp-ci"], true))
            .online(mac)
            .build();
        let mut reserved = Reserved::default();
        assert_eq!(
            place_run(&view, &settings, LOGIN, &mut reserved),
            Err(vec!["mac: no token for rawphp".to_owned()])
        );
    }

    #[test]
    fn an_offline_machine_is_said_to_be_offline_whatever_it_last_said_it_holds() {
        let (mac, vps, settings) = two();
        let view = FleetView::builder()
            .sample(sample(mac, 0, 0.1))
            .sample(sample(vps, 0, 0.1))
            .held(mac, holds(&["someone-else"], false))
            .build();
        let mut reserved = Reserved::default();
        assert_eq!(
            place_run(&view, &settings, LOGIN, &mut reserved),
            Err(vec!["mac: offline".to_owned(), "vps: offline".to_owned()])
        );
    }

    #[test]
    fn the_reasons_come_in_the_order_of_the_samples() {
        // Ids that sort the other way from the samples, so an order by id would show.
        let mac = MachineId(uuid::Uuid::from_u128(2));
        let vps = MachineId(uuid::Uuid::from_u128(1));
        let settings = settings(&[(mac, "mac"), (vps, "vps")]);
        let view = FleetView::builder()
            .sample(sample(mac, 4, 0.1))
            .sample(sample(vps, 0, 0.1))
            .held(mac, holds(&[LOGIN], true))
            .held(vps, holds(&["someone-else"], true))
            .online(mac)
            .online(vps)
            .build();
        let mut reserved = Reserved::default();
        assert_eq!(
            place_run(&view, &settings, LOGIN, &mut reserved),
            Err(vec![
                "mac: sessions 4 at limit 4".to_owned(),
                "vps: no token for rawphp".to_owned(),
            ])
        );
    }

    #[test]
    fn a_run_held_on_a_machine_counts_it_one_session_fuller() {
        let (mac, vps, settings) = two();
        // A run placed on the mac in an earlier pass, or by another project's pass, whose
        // session no sample counts yet.
        let mut reserved = Reserved::default();
        reserved.hold(mac);
        let view = FleetView::builder()
            .sample(sample(mac, 3, 0.1))
            .sample(sample(vps, 0, 0.5))
            .held(mac, holds(&[LOGIN], true))
            .held(vps, holds(&[LOGIN], true))
            .online(mac)
            .online(vps)
            .build();
        assert_eq!(
            place_run(&view, &settings, LOGIN, &mut reserved),
            Ok(Target::Machine(vps))
        );

        let mut reserved = Reserved::default();
        reserved.hold(mac);
        let alone = FleetView::builder()
            .sample(sample(mac, 3, 0.1))
            .held(mac, holds(&[LOGIN], true))
            .online(mac)
            .build();
        assert_eq!(
            place_run(&alone, &settings, LOGIN, &mut reserved),
            Err(vec!["mac: sessions 4 at limit 4".to_owned()])
        );
    }

    #[test]
    fn runs_held_and_placed_on_one_machine_add_up() {
        let (mac, vps, settings) = two();
        let alone = FleetView::builder()
            .sample(sample(mac, 1, 0.1))
            .held(mac, holds(&[LOGIN], true))
            .online(mac)
            .build();
        // Two runs held on the mac at 1 of 4 sessions leave it one: the next claim takes it,
        // and the one after finds it full.
        let mut reserved = Reserved::default();
        reserved.hold(mac);
        reserved.hold(mac);
        assert_eq!(
            place_run(&alone, &settings, LOGIN, &mut reserved),
            Ok(Target::Machine(mac))
        );
        assert_eq!(
            place_run(&alone, &settings, LOGIN, &mut reserved),
            Err(vec!["mac: sessions 4 at limit 4".to_owned()])
        );

        // Two claims placed in one pass add up the same way.
        let view = FleetView::builder()
            .sample(sample(mac, 2, 0.1))
            .sample(sample(vps, 0, 0.5))
            .held(mac, holds(&[LOGIN], true))
            .held(vps, holds(&[LOGIN], true))
            .online(mac)
            .online(vps)
            .build();
        let mut reserved = Reserved::default();
        assert_eq!(
            place_run(&view, &settings, LOGIN, &mut reserved),
            Ok(Target::Machine(mac))
        );
        assert_eq!(
            place_run(&view, &settings, LOGIN, &mut reserved),
            Ok(Target::Machine(mac))
        );
        assert_eq!(
            place_run(&view, &settings, LOGIN, &mut reserved),
            Ok(Target::Machine(vps))
        );
    }

    #[test]
    fn runs_held_on_one_machine_leave_another_as_full_as_its_sample() {
        let (mac, vps, settings) = two();
        // The vps has one session free and the lower load; the runs held are on the mac.
        let mut reserved = Reserved::default();
        reserved.hold(mac);
        reserved.hold(mac);
        let view = FleetView::builder()
            .sample(sample(mac, 0, 0.5))
            .sample(sample(vps, 3, 0.1))
            .held(mac, holds(&[LOGIN], true))
            .held(vps, holds(&[LOGIN], true))
            .online(mac)
            .online(vps)
            .build();
        assert_eq!(
            place_run(&view, &settings, LOGIN, &mut reserved),
            Ok(Target::Machine(vps))
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

//! Asks the local daemon for its state and prints it. Filled by plan task 20.

use std::process::ExitCode;

/// `foundryd status`.
pub fn status() -> ExitCode {
    crate::not_built("status")
}

#[cfg(test)]
mod tests {
    use foundry_proto::ids::{MachineId, SessionId, TerminalId, WorkspaceId};
    use foundry_proto::record::{AnyRecord, MachineSample, Record, Session, Terminal};
    use foundry_proto::settings::{Machine, SharedSettings};
    use foundry_proto::state::{SessionState, TerminalState};

    use super::*;

    fn sample(owner: MachineId, version: u64, load: f64, sessions: u32) -> AnyRecord {
        AnyRecord::MachineSample(Record {
            owner,
            version,
            deleted: false,
            body: MachineSample {
                load_per_core: load,
                free_memory: 12_300_000_000,
                free_disk: 120_500_000_000,
                sessions,
            },
        })
    }

    fn session(owner: MachineId, state: SessionState, deleted: bool) -> AnyRecord {
        AnyRecord::Session(Record {
            owner,
            version: 5,
            deleted,
            body: Session {
                id: SessionId::random(),
                workspace: WorkspaceId::random(),
                terminal: TerminalId::random(),
                model: "opus".into(),
                state,
            },
        })
    }

    fn terminal(owner: MachineId, state: TerminalState) -> AnyRecord {
        AnyRecord::Terminal(Record {
            owner,
            version: 5,
            deleted: false,
            body: Terminal {
                id: TerminalId::random(),
                workspace: WorkspaceId::random(),
                state,
                exit_code: None,
            },
        })
    }

    fn settings(machines: &[(MachineId, &str)]) -> SharedSettings {
        SharedSettings {
            machines: machines
                .iter()
                .map(|(id, name)| Machine {
                    id: *id,
                    name: (*name).into(),
                    address: "100.64.0.1".into(),
                })
                .collect(),
            ..SharedSettings::default()
        }
    }

    #[test]
    fn a_named_machine_shows_its_name_state_and_numbers() {
        let mac = MachineId::random();
        let out = render(
            &[sample(mac, 3, 0.4242, 2)],
            &settings(&[(mac, "trilogy-mac")]),
        );

        assert!(out.starts_with("trilogy-mac"), "{out}");
        assert!(out.contains("online"), "{out}");
        assert!(out.contains("load 0.42/core"), "{out}");
        assert!(out.contains("memory 12.3 GB free"), "{out}");
        assert!(out.contains("disk 120.5 GB free"), "{out}");
        assert!(out.contains("2 sessions"), "{out}");
    }

    #[test]
    fn a_machine_not_in_the_settings_shows_its_id() {
        let stranger = MachineId::random();
        let out = render(&[sample(stranger, 1, 0.1, 0)], &settings(&[]));

        assert!(out.starts_with(&stranger.to_string()), "{out}");
    }

    #[test]
    fn the_newest_sample_is_the_one_shown() {
        let mac = MachineId::random();
        let records = [sample(mac, 9, 0.9, 7), sample(mac, 2, 0.1, 1)];
        let out = render(&records, &settings(&[(mac, "mac")]));

        assert!(out.contains("load 0.90/core"), "{out}");
        assert!(!out.contains("load 0.10/core"), "{out}");
    }

    #[test]
    fn a_machine_with_no_sample_yet_says_so() {
        let mac = MachineId::random();
        let out = render(&[], &settings(&[(mac, "mac")]));

        assert!(out.contains("no sample yet"), "{out}");
    }

    #[test]
    fn an_unreachable_session_or_terminal_shows_the_machine_offline() {
        let (a, b) = (MachineId::random(), MachineId::random());
        let records = [
            sample(a, 1, 0.1, 1),
            session(a, SessionState::Unreachable, false),
            sample(b, 1, 0.1, 0),
            terminal(b, TerminalState::Unreachable),
        ];
        let out = render(&records, &settings(&[(a, "alpha"), (b, "beta")]));

        for line in out
            .lines()
            .filter(|l| l.starts_with("alpha") || l.starts_with("beta"))
        {
            assert!(line.contains("offline"), "{line}");
        }
        assert!(out.contains("alpha"), "{out}");
        assert!(out.contains("beta"), "{out}");
    }

    #[test]
    fn a_machine_with_only_reachable_records_is_online() {
        let mac = MachineId::random();
        let records = [
            sample(mac, 1, 0.1, 1),
            session(mac, SessionState::Idle, false),
            terminal(mac, TerminalState::Running),
        ];
        let out = render(&records, &settings(&[(mac, "mac")]));

        assert!(out.contains("online"), "{out}");
        assert!(!out.contains("offline"), "{out}");
    }

    #[test]
    fn sessions_are_listed_under_their_machine_with_state_and_model() {
        let mac = MachineId::random();
        let working = session(mac, SessionState::WaitingForYou, false);
        let gone = session(mac, SessionState::Idle, true);
        let AnyRecord::Session(shown) = &working else {
            unreachable!()
        };
        let id = shown.body.id.to_string();
        let out = render(
            &[sample(mac, 1, 0.1, 1), working, gone],
            &settings(&[(mac, "mac")]),
        );

        let line = out
            .lines()
            .find(|l| l.contains(&id))
            .unwrap_or_else(|| panic!("no line for the session: {out}"));
        assert!(line.starts_with("  "), "{line}");
        assert!(line.contains("waiting_for_you"), "{line}");
        assert!(line.contains("opus"), "{line}");
        assert_eq!(
            out.matches("session ").count(),
            1,
            "the deleted one is hidden: {out}"
        );
    }

    #[test]
    fn machines_come_in_settings_order_then_by_id() {
        let (first, second) = (MachineId::random(), MachineId::random());
        let (late_a, late_b) = (MachineId::random(), MachineId::random());
        let (low, high) = if late_a < late_b {
            (late_a, late_b)
        } else {
            (late_b, late_a)
        };
        let records = [
            sample(high, 1, 0.1, 0),
            sample(second, 1, 0.1, 0),
            sample(low, 1, 0.1, 0),
        ];
        let out = render(&records, &settings(&[(first, "first"), (second, "second")]));
        let at = |needle: &str| {
            out.find(needle)
                .unwrap_or_else(|| panic!("{needle} in {out}"))
        };

        assert!(at("first") < at("second"));
        assert!(at("second") < at(&low.to_string()));
        assert!(at(&low.to_string()) < at(&high.to_string()));
    }
}

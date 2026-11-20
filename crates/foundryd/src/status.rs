//! `foundryd status`: asks the local daemon for its state and prints each machine with its
//! numbers and each of its sessions.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io;
use std::process::ExitCode;
use std::time::Duration;

use foundry_proto::ids::MachineId;
use foundry_proto::messages::UiMessage;
use foundry_proto::record::{AnyRecord, MachineSample, Record};
use foundry_proto::settings::SharedSettings;
use foundry_proto::state::{SessionState, TerminalState};

/// How long the daemon has to answer.
const ANSWER_WITHIN: Duration = Duration::from_secs(5);

/// `foundryd status`.
pub fn status() -> ExitCode {
    let port = match crate::daemon::port() {
        Ok(port) => port,
        Err(error) => {
            eprintln!("foundryd status: {error}");
            return ExitCode::FAILURE;
        }
    };
    match fetch(port) {
        Ok(UiMessage::State {
            records, settings, ..
        }) => {
            print!("{}", render(&records, &settings));
            ExitCode::SUCCESS
        }
        Ok(_) => {
            eprintln!("foundryd status: the daemon on 127.0.0.1:{port} did not send its state");
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("foundryd status: {error}");
            ExitCode::FAILURE
        }
    }
}

/// `GET /api/state` from the daemon on loopback at `port`.
fn fetch(port: u16) -> Result<UiMessage, String> {
    let url = format!("http://127.0.0.1:{port}/api/state");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(ANSWER_WITHIN))
        .build()
        .into();
    match agent.get(&url).call() {
        Ok(mut response) => response
            .body_mut()
            .read_json()
            .map_err(|error| format!("the answer from {url} is not the state: {error}")),
        Err(ureq::Error::StatusCode(status)) => Err(format!("{url} answered {status}")),
        Err(ureq::Error::Io(error)) if error.kind() == io::ErrorKind::ConnectionRefused => {
            Err(format!("no daemon is running on 127.0.0.1:{port}"))
        }
        Err(ureq::Error::ConnectionFailed) => {
            Err(format!("no daemon is running on 127.0.0.1:{port}"))
        }
        Err(error) => Err(format!("cannot read {url}: {error}")),
    }
}

/// Every machine and its sessions as plain text: machines in the settings' order, then any
/// other machine that has a sample, by id.
pub fn render(records: &[AnyRecord], settings: &SharedSettings) -> String {
    let mut machines: Vec<MachineId> = settings.machines.iter().map(|m| m.id).collect();
    let listed: BTreeSet<MachineId> = machines.iter().copied().collect();
    let others: BTreeSet<MachineId> = records
        .iter()
        .filter_map(|record| match record {
            AnyRecord::MachineSample(sample) if !listed.contains(&sample.owner) => {
                Some(sample.owner)
            }
            _ => None,
        })
        .collect();
    machines.extend(others);

    let mut out = String::new();
    for id in machines {
        let name = settings
            .machines
            .iter()
            .find(|m| m.id == id)
            .map_or_else(|| id.to_string(), |m| m.name.clone());
        machine_lines(&mut out, &name, id, records);
    }
    out
}

/// The machine's line, then a line for each of its sessions that is not deleted.
fn machine_lines(out: &mut String, name: &str, id: MachineId, records: &[AnyRecord]) {
    let offline = records.iter().any(|record| match record {
        AnyRecord::Session(r) => r.owner == id && r.body.state == SessionState::Unreachable,
        AnyRecord::Terminal(r) => r.owner == id && r.body.state == TerminalState::Unreachable,
        _ => false,
    });
    let state = if offline { "offline" } else { "online" };
    let newest: Option<&Record<MachineSample>> = records
        .iter()
        .filter_map(|record| match record {
            AnyRecord::MachineSample(sample) if sample.owner == id => Some(sample),
            _ => None,
        })
        .max_by_key(|sample| sample.version);
    let numbers = match newest {
        Some(sample) => {
            let body = &sample.body;
            let plural = if body.sessions == 1 { "" } else { "s" };
            format!(
                "load {:.2}/core  memory {:.1} GB free  disk {:.1} GB free  {} session{plural}",
                body.load_per_core,
                gigabytes(body.free_memory),
                gigabytes(body.free_disk),
                body.sessions
            )
        }
        None => "no sample yet".to_owned(),
    };
    let _ = writeln!(out, "{name}  {state}  {numbers}");
    for record in records {
        if let AnyRecord::Session(r) = record
            && r.owner == id
            && !r.deleted
        {
            let _ = writeln!(
                out,
                "  session {}  {}  {}",
                r.body.id,
                wire_name(r.body.state),
                r.body.model
            );
        }
    }
}

/// Decimal gigabytes.
fn gigabytes(bytes: u64) -> f64 {
    bytes as f64 / 1e9
}

/// A state as it is on the wire, such as `waiting_for_you`.
fn wire_name(state: SessionState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{state:?}"))
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

//! `foundryd status`: asks the local daemon for its state and prints each machine with its
//! numbers and each of its sessions.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io;
use std::process::ExitCode;
use std::time::Duration;

use foundry_proto::ids::MachineId;
use foundry_proto::messages::UiMessage;
use foundry_proto::record::{AnyRecord, MachineSample, Record, Session};
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
            records,
            settings,
            online,
            ..
        }) => {
            print!("{}", render(&records, &settings, online.as_deref()));
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
    let (url, mut response) = get(port, "/api/state")?;
    response
        .body_mut()
        .read_json()
        .map_err(|error| format!("the answer from {url} is not the state: {error}"))
}

/// `GET path` from the daemon on loopback at `port`, with the URL asked. An answer other than
/// `2xx`, and no daemon on the port, are errors that say so.
pub(crate) fn get(
    port: u16,
    path: &str,
) -> Result<(String, ureq::http::Response<ureq::Body>), String> {
    let url = format!("http://127.0.0.1:{port}{path}");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(ANSWER_WITHIN))
        .build()
        .into();
    match agent.get(&url).call() {
        Ok(response) => Ok((url, response)),
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

#[derive(Default)]
struct MachineSummary<'a> {
    has_reachable: bool,
    has_unreachable: bool,
    newest_sample: Option<&'a Record<MachineSample>>,
    live_sessions: Vec<&'a Record<Session>>,
}

/// Every machine and its sessions as plain text: machines in the settings' order, then any
/// other machine that has a sample, by id. With `online`, the machines the daemon hears, a
/// machine is `online` when the set holds it and `offline` when it does not, whatever its
/// records say. Without it, from a daemon whose state carries no online set, a machine's state
/// is shown only where its records prove it: `offline` when one is marked unreachable,
/// `online` when one is not, `unknown` when it has none.
pub fn render(
    records: &[AnyRecord],
    settings: &SharedSettings,
    online: Option<&[MachineId]>,
) -> String {
    let mut names: BTreeMap<MachineId, &str> = BTreeMap::new();
    let mut machines: Vec<MachineId> = Vec::with_capacity(settings.machines.len());
    for machine in &settings.machines {
        machines.push(machine.id);
        names.insert(machine.id, machine.name.as_str());
    }

    let mut by_machine: BTreeMap<MachineId, MachineSummary<'_>> = BTreeMap::new();
    let mut others: BTreeSet<MachineId> = BTreeSet::new();
    for record in records {
        match record {
            AnyRecord::MachineSample(sample) => {
                if !names.contains_key(&sample.owner) {
                    others.insert(sample.owner);
                }
                let entry = by_machine.entry(sample.owner).or_default();
                if entry
                    .newest_sample
                    .is_none_or(|prev| sample.version > prev.version)
                {
                    entry.newest_sample = Some(sample);
                }
            }
            AnyRecord::Session(r) => {
                let entry = by_machine.entry(r.owner).or_default();
                if r.body.state == SessionState::Unreachable {
                    entry.has_unreachable = true;
                } else {
                    entry.has_reachable = true;
                }
                if !r.deleted {
                    entry.live_sessions.push(r);
                }
            }
            AnyRecord::Terminal(r) => {
                let entry = by_machine.entry(r.owner).or_default();
                if r.body.state == TerminalState::Unreachable {
                    entry.has_unreachable = true;
                } else {
                    entry.has_reachable = true;
                }
            }
            AnyRecord::Workspace(_) => {}
        }
    }
    machines.extend(others);

    let mut out = String::new();
    for id in machines {
        let fallback;
        let name = match names.get(&id) {
            Some(name) => *name,
            None => {
                fallback = id.to_string();
                &fallback
            }
        };
        let summary = by_machine.get(&id);
        let state = match (online, summary) {
            (Some(online), _) if online.contains(&id) => "online",
            (Some(_), _) => "offline",
            (None, Some(s)) if s.has_unreachable => "offline",
            (None, Some(s)) if s.has_reachable => "online",
            (None, _) => "unknown",
        };
        write_machine_lines(&mut out, name, state, summary);
    }
    out
}

/// The machine's line, then a line for each of its sessions that is not deleted.
fn write_machine_lines(
    out: &mut String,
    name: &str,
    state: &str,
    summary: Option<&MachineSummary<'_>>,
) {
    match summary.and_then(|s| s.newest_sample) {
        Some(sample) => {
            let body = &sample.body;
            let plural = if body.sessions == 1 { "" } else { "s" };
            let _ = writeln!(
                out,
                "{name}  {state}  load {:.2}/core  memory {:.1} GB free  disk {:.1} GB free  {} session{plural}",
                body.load_per_core,
                gigabytes(body.free_memory),
                gigabytes(body.free_disk),
                body.sessions
            );
        }
        None => {
            let _ = writeln!(out, "{name}  {state}  no sample yet");
        }
    }
    if let Some(summary) = summary {
        for r in &summary.live_sessions {
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
fn wire_name(state: SessionState) -> &'static str {
    match state {
        SessionState::Starting => "starting",
        SessionState::Working => "working",
        SessionState::WaitingForYou => "waiting_for_you",
        SessionState::Idle => "idle",
        SessionState::Finished => "finished",
        SessionState::Failed => "failed",
        SessionState::Stuck => "stuck",
        SessionState::Unreachable => "unreachable",
    }
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
                update_refused: None,
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
                    key: String::new(),
                })
                .collect(),
            ..SharedSettings::default()
        }
    }

    #[test]
    fn a_named_machine_shows_its_name_state_and_numbers() {
        let mac = MachineId::random();
        let out = render(
            &[
                sample(mac, 3, 0.4242, 2),
                terminal(mac, TerminalState::Running),
            ],
            &settings(&[(mac, "trilogy-mac")]),
            None,
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
        let out = render(&[sample(stranger, 1, 0.1, 0)], &settings(&[]), None);

        assert!(out.starts_with(&stranger.to_string()), "{out}");
    }

    #[test]
    fn the_newest_sample_is_the_one_shown() {
        let mac = MachineId::random();
        let records = [sample(mac, 9, 0.9, 7), sample(mac, 2, 0.1, 1)];
        let out = render(&records, &settings(&[(mac, "mac")]), None);

        assert!(out.contains("load 0.90/core"), "{out}");
        assert!(!out.contains("load 0.10/core"), "{out}");
    }

    #[test]
    fn a_machine_with_no_sample_yet_says_so() {
        let mac = MachineId::random();
        let out = render(&[], &settings(&[(mac, "mac")]), None);

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
        let out = render(&records, &settings(&[(a, "alpha"), (b, "beta")]), None);

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
    fn a_machine_with_a_reachable_record_is_online() {
        let mac = MachineId::random();
        let records = [
            sample(mac, 1, 0.1, 1),
            session(mac, SessionState::Idle, false),
            terminal(mac, TerminalState::Running),
        ];
        let out = render(&records, &settings(&[(mac, "mac")]), None);

        assert!(out.contains("online"), "{out}");
        assert!(!out.contains("offline"), "{out}");
    }

    #[test]
    fn a_listed_machine_with_only_a_sample_is_not_called_online() {
        let peer = MachineId::random();
        let out = render(
            &[sample(peer, 1, 0.1, 0)],
            &settings(&[(peer, "peer")]),
            None,
        );

        assert!(out.contains("unknown"), "{out}");
        assert!(!out.contains("online"), "{out}");
        assert!(!out.contains("offline"), "{out}");
    }

    #[test]
    fn with_the_online_set_a_listed_machine_with_only_a_sample_is_online_or_offline() {
        let peer = MachineId::random();
        let records = [sample(peer, 1, 0.1, 0)];
        let listed = settings(&[(peer, "peer")]);

        let heard = render(&records, &listed, Some(&[peer]));
        assert!(heard.contains("peer  online"), "{heard}");
        assert!(!heard.contains("unknown"), "{heard}");

        let silent = render(&records, &listed, Some(&[]));
        assert!(silent.contains("peer  offline"), "{silent}");
        assert!(!silent.contains("unknown"), "{silent}");
    }

    #[test]
    fn with_the_online_set_records_do_not_decide_the_state() {
        let (up, down, stranger, silent) = (
            MachineId::random(),
            MachineId::random(),
            MachineId::random(),
            MachineId::random(),
        );
        let records = [
            terminal(up, TerminalState::Unreachable),
            terminal(down, TerminalState::Running),
            sample(stranger, 1, 0.1, 0),
            sample(silent, 1, 0.1, 0),
        ];
        let out = render(
            &records,
            &settings(&[(up, "up"), (down, "down")]),
            Some(&[up, stranger]),
        );

        assert!(out.contains("up  online"), "{out}");
        assert!(out.contains("down  offline"), "{out}");
        assert!(out.contains(&format!("{stranger}  online")), "{out}");
        assert!(out.contains(&format!("{silent}  offline")), "{out}");
    }

    #[test]
    fn the_state_the_daemon_sent_is_shown_by_its_online_set() {
        let me = MachineId::random();
        let state = |online| UiMessage::State {
            protocol: foundry_proto::messages::PROTOCOL,
            records: vec![sample(me, 1, 0.1, 0)],
            settings: settings(&[(me, "mac")]),
            online,
        };

        let heard = state_text(state(Some(vec![me]))).unwrap();
        assert!(heard.contains("mac  online"), "{heard}");
        let older = state_text(state(None)).unwrap();
        assert!(older.contains("mac  unknown"), "{older}");
        let record = UiMessage::Settings {
            protocol: foundry_proto::messages::PROTOCOL,
            settings: settings(&[]),
        };
        assert!(state_text(record).is_none());
    }

    #[test]
    fn a_listed_machine_with_no_records_is_unknown() {
        let peer = MachineId::random();
        let out = render(&[], &settings(&[(peer, "peer")]), None);

        assert!(out.contains("unknown"), "{out}");
    }

    #[test]
    fn one_unreachable_record_makes_the_machine_offline_despite_a_reachable_one() {
        let mac = MachineId::random();
        let records = [
            session(mac, SessionState::Idle, false),
            terminal(mac, TerminalState::Unreachable),
        ];
        let out = render(&records, &settings(&[(mac, "mac")]), None);

        assert!(out.contains("offline"), "{out}");
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
            None,
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
        let out = render(
            &records,
            &settings(&[(first, "first"), (second, "second")]),
            None,
        );
        let at = |needle: &str| {
            out.find(needle)
                .unwrap_or_else(|| panic!("{needle} in {out}"))
        };

        assert!(at("first") < at("second"));
        assert!(at("second") < at(&low.to_string()));
        assert!(at(&low.to_string()) < at(&high.to_string()));
    }

    #[test]
    fn wire_name_matches_serde_for_every_session_state() {
        for state in [
            SessionState::Starting,
            SessionState::Working,
            SessionState::WaitingForYou,
            SessionState::Idle,
            SessionState::Finished,
            SessionState::Failed,
            SessionState::Stuck,
            SessionState::Unreachable,
        ] {
            let expected = serde_json::to_value(state).unwrap();
            assert_eq!(wire_name(state), expected.as_str().unwrap());
        }
    }
}

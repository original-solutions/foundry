//! Peer and UI messages, each carrying a protocol number.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ids::{MachineId, TerminalId};
use crate::record::AnyRecord;
use crate::settings::SharedSettings;

/// The protocol number this build speaks.
pub const PROTOCOL: u16 = 1;

/// A message between two machines' daemons, tagged by `type`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PeerMessage {
    /// Opens a link: who is speaking and the highest version it holds per owner.
    Hello {
        protocol: u16,
        machine: MachineId,
        versions: BTreeMap<MachineId, u64>,
    },
    /// Records the receiver may not have yet.
    Records {
        protocol: u16,
        records: Vec<AnyRecord>,
    },
    /// The shared settings as the sender holds them.
    Settings {
        protocol: u16,
        settings: SharedSettings,
    },
    /// Opens a link, in place of a hello, that carries one of the receiver's terminals to the
    /// sender: who is speaking and which terminal.
    Relay {
        protocol: u16,
        machine: MachineId,
        terminal: TerminalId,
    },
}

impl PeerMessage {
    pub fn protocol(&self) -> u16 {
        match self {
            Self::Hello { protocol, .. }
            | Self::Records { protocol, .. }
            | Self::Settings { protocol, .. }
            | Self::Relay { protocol, .. } => *protocol,
        }
    }
}

/// A message from the daemon to the UI, tagged by `type`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UiMessage {
    /// The full state, sent when the UI connects.
    State {
        protocol: u16,
        records: Vec<AnyRecord>,
        settings: SharedSettings,
    },
    /// One record changed.
    Record { protocol: u16, record: AnyRecord },
    /// The shared settings changed.
    Settings {
        protocol: u16,
        settings: SharedSettings,
    },
}

impl UiMessage {
    pub fn protocol(&self) -> u16 {
        match self {
            Self::State { protocol, .. }
            | Self::Record { protocol, .. }
            | Self::Settings { protocol, .. } => *protocol,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::tests::{machine_sample, session, terminal, workspace};
    use crate::settings::tests::settings;
    use crate::settings::{Edit, SettingKey};
    use crate::{assert_round_trip, parse_error};

    fn records() -> Vec<AnyRecord> {
        vec![
            AnyRecord::Workspace(workspace()),
            AnyRecord::Terminal(terminal()),
            AnyRecord::Session(session()),
            AnyRecord::MachineSample(machine_sample()),
        ]
    }

    fn peer_messages() -> Vec<PeerMessage> {
        vec![
            PeerMessage::Hello {
                protocol: PROTOCOL,
                machine: MachineId::random(),
                versions: [(MachineId::random(), 9), (MachineId::random(), 0)].into(),
            },
            PeerMessage::Records {
                protocol: PROTOCOL,
                records: records(),
            },
            PeerMessage::Settings {
                protocol: PROTOCOL,
                settings: settings(),
            },
            PeerMessage::Relay {
                protocol: PROTOCOL,
                machine: MachineId::random(),
                terminal: TerminalId::random(),
            },
            PeerMessage::Edit {
                protocol: PROTOCOL,
                edit: Edit::SetProject {
                    project: settings().projects[0].clone(),
                },
            },
            PeerMessage::Edit {
                protocol: PROTOCOL,
                edit: Edit::Dismiss {
                    setting: SettingKey::Priority,
                },
            },
        ]
    }

    fn ui_messages() -> Vec<UiMessage> {
        vec![
            UiMessage::State {
                protocol: PROTOCOL,
                records: records(),
                settings: settings(),
            },
            UiMessage::Record {
                protocol: PROTOCOL,
                record: AnyRecord::Session(session()),
            },
            UiMessage::Settings {
                protocol: PROTOCOL,
                settings: settings(),
            },
        ]
    }

    #[test]
    fn every_peer_message_round_trips() {
        for message in peer_messages() {
            assert_round_trip(&message);
        }
    }

    #[test]
    fn every_ui_message_round_trips() {
        for message in ui_messages() {
            assert_round_trip(&message);
        }
    }

    #[test]
    fn every_message_carries_its_protocol_on_the_wire() {
        for message in peer_messages() {
            assert_eq!(message.protocol(), PROTOCOL);
            assert_eq!(
                serde_json::to_value(&message).unwrap()["protocol"],
                PROTOCOL
            );
        }
        for message in ui_messages() {
            assert_eq!(message.protocol(), PROTOCOL);
            assert_eq!(
                serde_json::to_value(&message).unwrap()["protocol"],
                PROTOCOL
            );
        }
    }

    #[test]
    fn a_message_keeps_the_protocol_it_was_sent_with() {
        let peer: PeerMessage =
            serde_json::from_str(r#"{"type":"records","protocol":7,"records":[]}"#).unwrap();
        assert_eq!(peer.protocol(), 7);
        let ui: UiMessage = serde_json::from_str(
            r#"{"type":"settings","protocol":9,"settings":{"seq":0,"machines":[],"priority":[],"projects":[],"limits":{}}}"#,
        )
        .unwrap();
        assert_eq!(ui.protocol(), 9);
    }

    #[test]
    fn every_message_reads_from_its_wire_form() {
        let machine = "67e55044-10b1-426f-9247-bb680e5fe0c8";
        let settings = r#"{"seq":0,"machines":[],"priority":[],"projects":[],"limits":{}}"#;
        let sample = format!(
            r#"{{"kind":"machine_sample","owner":"{machine}","version":1,"deleted":false,"body":{{"load_per_core":0.5,"free_memory":1,"free_disk":2,"sessions":0}}}}"#
        );
        let record: AnyRecord = serde_json::from_str(&sample).unwrap();
        let id = MachineId(machine.parse().unwrap());
        let terminal = "1b4e28ba-2fa1-11d2-883f-0016d3cca427";
        let peer = [
            (
                format!(
                    r#"{{"type":"hello","protocol":1,"machine":"{machine}","versions":{{"{machine}":4}}}}"#
                ),
                PeerMessage::Hello {
                    protocol: 1,
                    machine: id,
                    versions: [(id, 4)].into(),
                },
            ),
            (
                format!(r#"{{"type":"records","protocol":1,"records":[{sample}]}}"#),
                PeerMessage::Records {
                    protocol: 1,
                    records: vec![record.clone()],
                },
            ),
            (
                format!(r#"{{"type":"settings","protocol":1,"settings":{settings}}}"#),
                PeerMessage::Settings {
                    protocol: 1,
                    settings: SharedSettings::default(),
                },
            ),
            (
                format!(
                    r#"{{"type":"relay","protocol":1,"machine":"{machine}","terminal":"{terminal}"}}"#
                ),
                PeerMessage::Relay {
                    protocol: 1,
                    machine: id,
                    terminal: TerminalId(terminal.parse().unwrap()),
                },
            ),
            (
                format!(
                    r#"{{"type":"edit","protocol":1,"edit":{{"kind":"remove_limits","machine":"{machine}"}}}}"#
                ),
                PeerMessage::Edit {
                    protocol: 1,
                    edit: Edit::RemoveLimits { machine: id },
                },
            ),
        ];
        for (json, expected) in peer {
            assert_eq!(
                serde_json::from_str::<PeerMessage>(&json).expect(&json),
                expected
            );
        }
        let ui = [
            (
                format!(
                    r#"{{"type":"state","protocol":1,"records":[{sample}],"settings":{settings}}}"#
                ),
                UiMessage::State {
                    protocol: 1,
                    records: vec![record.clone()],
                    settings: SharedSettings::default(),
                },
            ),
            (
                format!(r#"{{"type":"record","protocol":1,"record":{sample}}}"#),
                UiMessage::Record {
                    protocol: 1,
                    record: record.clone(),
                },
            ),
            (
                format!(r#"{{"type":"settings","protocol":1,"settings":{settings}}}"#),
                UiMessage::Settings {
                    protocol: 1,
                    settings: SharedSettings::default(),
                },
            ),
        ];
        for (json, expected) in ui {
            assert_eq!(
                serde_json::from_str::<UiMessage>(&json).expect(&json),
                expected
            );
        }
    }

    #[test]
    fn an_unknown_field_is_ignored() {
        for message in peer_messages() {
            let mut json = serde_json::to_value(&message).unwrap();
            json["added_later"] = serde_json::json!("x");
            let back: PeerMessage = serde_json::from_value(json).unwrap();
            assert_eq!(back, message);
        }
        for message in ui_messages() {
            let mut json = serde_json::to_value(&message).unwrap();
            json["added_later"] = serde_json::json!("x");
            let back: UiMessage = serde_json::from_value(json).unwrap();
            assert_eq!(back, message);
        }
    }

    #[test]
    fn an_unknown_message_type_is_an_error_that_names_it() {
        let peer = parse_error::<PeerMessage>(r#"{"type":"gossip","protocol":1}"#);
        assert!(peer.contains("gossip"), "{peer}");
        let ui = parse_error::<UiMessage>(r#"{"type":"confetti","protocol":1}"#);
        assert!(ui.contains("confetti"), "{ui}");
    }
}

//! Peer and UI messages, each carrying a protocol number.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ids::{MachineId, TerminalId};
use crate::record::AnyRecord;
use crate::settings::{Edit, EditId, Outcome, SharedSettings};

/// The protocol number this build speaks.
pub const PROTOCOL: u16 = 4;

/// The first protocol whose links carry the shared settings and edits. A link in an older
/// protocol carries neither: a machine on it has no coordinator, and refuses a message whose
/// `type` it does not know.
pub const EDITS_SINCE: u16 = 2;

/// The first protocol that carries a [`PeerMessage::Call`]. A machine on an older link is never
/// sent one: it would refuse a link that opens with a `type` it does not know.
pub const CALLS_SINCE: u16 = 3;

/// The first protocol whose edits carry an id and are answered. A machine sends an edit with an
/// id only on a link in it or later, and an answer never on an older one. An edit with no id,
/// passed on from a machine before this protocol, still goes on any link in [`EDITS_SINCE`] or
/// later.
pub const ANSWERS_SINCE: u16 = 4;

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
    /// An edit to the shared settings, sent to the acting coordinator to number. Older
    /// protocols send no id.
    Edit {
        protocol: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<EditId>,
        edit: Edit,
    },
    /// The acting coordinator's answer to the edit sent under `id`: numbered or refused.
    Answer {
        protocol: u16,
        id: EditId,
        outcome: Outcome,
    },
    /// Opens a link, in place of a hello, that carries one call to the receiver's API and its
    /// answer: who is speaking and the call.
    Call {
        protocol: u16,
        machine: MachineId,
        call: Call,
    },
}

/// A `POST` to another machine's API, carried to it as it was made.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Call {
    /// The path, such as `/api/workspaces`.
    pub path: String,
    /// The request's `Content-Type`, when it had one.
    pub content_type: Option<String>,
    /// The request body's bytes, as hex.
    pub body: String,
}

impl PeerMessage {
    pub fn protocol(&self) -> u16 {
        match self {
            Self::Hello { protocol, .. }
            | Self::Records { protocol, .. }
            | Self::Settings { protocol, .. }
            | Self::Relay { protocol, .. }
            | Self::Edit { protocol, .. }
            | Self::Answer { protocol, .. }
            | Self::Call { protocol, .. } => *protocol,
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
    use crate::settings::{EditId, Outcome, SettingKey, Stamp};
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
                id: Some(EditId::random()),
                edit: Edit::SetProject {
                    project: settings().projects[0].clone(),
                },
            },
            PeerMessage::Edit {
                protocol: PROTOCOL,
                id: None,
                edit: Edit::Dismiss {
                    setting: SettingKey::Priority,
                },
            },
            PeerMessage::Answer {
                protocol: PROTOCOL,
                id: EditId::random(),
                outcome: Outcome::Numbered {
                    stamp: Stamp {
                        seq: 9,
                        by: MachineId::random(),
                    },
                },
            },
            PeerMessage::Answer {
                protocol: PROTOCOL,
                id: EditId::random(),
                outcome: Outcome::Refused {
                    reason: "it could not be stored".into(),
                    conflict: false,
                },
            },
            PeerMessage::Call {
                protocol: PROTOCOL,
                machine: MachineId::random(),
                call: Call {
                    path: "/api/workspaces".into(),
                    content_type: Some("application/json".into()),
                    body: "7b7d".into(),
                },
            },
        ]
    }

    #[test]
    fn calls_are_carried_from_the_protocol_after_edits() {
        assert_eq!(CALLS_SINCE, EDITS_SINCE + 1);
    }

    #[test]
    fn answers_are_carried_from_the_protocol_after_calls_and_this_build_speaks_it() {
        assert_eq!(ANSWERS_SINCE, CALLS_SINCE + 1);
        assert_eq!(PROTOCOL, ANSWERS_SINCE);
    }

    #[test]
    fn an_edit_without_an_id_is_written_as_the_older_protocols_wrote_it() {
        let edit = PeerMessage::Edit {
            protocol: CALLS_SINCE,
            id: None,
            edit: Edit::Dismiss {
                setting: SettingKey::Priority,
            },
        };
        let json = serde_json::to_value(&edit).unwrap();
        assert!(json.get("id").is_none(), "{json}");
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
        let edit = "22222222-2222-4222-8222-222222222222";
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
                    id: None,
                    edit: Edit::RemoveLimits { machine: id },
                },
            ),
            (
                format!(
                    r#"{{"type":"edit","protocol":4,"id":"{edit}","edit":{{"kind":"remove_limits","machine":"{machine}"}}}}"#
                ),
                PeerMessage::Edit {
                    protocol: 4,
                    id: Some(EditId(edit.parse().unwrap())),
                    edit: Edit::RemoveLimits { machine: id },
                },
            ),
            (
                format!(
                    r#"{{"type":"answer","protocol":4,"id":"{edit}","outcome":{{"kind":"numbered","stamp":{{"seq":3,"by":"{machine}"}}}}}}"#
                ),
                PeerMessage::Answer {
                    protocol: 4,
                    id: EditId(edit.parse().unwrap()),
                    outcome: Outcome::Numbered {
                        stamp: Stamp { seq: 3, by: id },
                    },
                },
            ),
            (
                format!(
                    r#"{{"type":"answer","protocol":4,"id":"{edit}","outcome":{{"kind":"refused","reason":"no"}}}}"#
                ),
                PeerMessage::Answer {
                    protocol: 4,
                    id: EditId(edit.parse().unwrap()),
                    outcome: Outcome::Refused {
                        reason: "no".into(),
                        conflict: false,
                    },
                },
            ),
            (
                format!(
                    r#"{{"type":"call","protocol":3,"machine":"{machine}","call":{{"path":"/api/terminals/{terminal}/close","content_type":null,"body":""}}}}"#
                ),
                PeerMessage::Call {
                    protocol: 3,
                    machine: id,
                    call: Call {
                        path: format!("/api/terminals/{terminal}/close"),
                        content_type: None,
                        body: String::new(),
                    },
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

//! Peer and UI messages, each carrying a protocol number.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::factory::{FactoryRecord, FactoryStatus};
use crate::ids::{MachineId, TerminalId};
use crate::record::AnyRecord;
use crate::settings::{Edit, EditId, Outcome, SharedSettings};

/// The protocol number this build speaks.
pub const PROTOCOL: u16 = 5;

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

/// The first protocol whose links carry factory records and factory settings. A machine sends
/// neither on a link in an older protocol: such a peer refuses a record kind, a message `type`
/// and a settings key it does not know. Its settings copy goes through
/// [`SharedSettings::for_protocol`].
pub const FACTORY_SINCE: u16 = 5;

/// A message between two machines' daemons, tagged by `type`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PeerMessage {
    /// Opens a link: who is speaking and the highest version it holds per owner, of P1's
    /// records and of factory records. A hello from before [`FACTORY_SINCE`] has no
    /// `factory_versions` and reads as holding none.
    Hello {
        protocol: u16,
        machine: MachineId,
        versions: BTreeMap<MachineId, u64>,
        #[serde(default)]
        factory_versions: BTreeMap<MachineId, u64>,
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
    /// Factory records the receiver may not have yet. Sent only on a link in
    /// [`FACTORY_SINCE`] or later.
    FactoryRecords {
        protocol: u16,
        records: Vec<FactoryRecord>,
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
            | Self::Call { protocol, .. }
            | Self::FactoryRecords { protocol, .. } => *protocol,
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
        /// Every machine the daemon serving the UI hears, itself included. None from a daemon
        /// that predates it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        online: Option<Vec<MachineId>>,
    },
    /// One record changed.
    Record { protocol: u16, record: AnyRecord },
    /// The shared settings changed.
    Settings {
        protocol: u16,
        settings: SharedSettings,
    },
    /// The status believed for one project's factory changed.
    FactoryStatus {
        protocol: u16,
        status: FactoryStatus,
    },
}

impl UiMessage {
    pub fn protocol(&self) -> u16 {
        match self {
            Self::State { protocol, .. }
            | Self::Record { protocol, .. }
            | Self::Settings { protocol, .. }
            | Self::FactoryStatus { protocol, .. } => *protocol,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::factory::tests::{records as factory_records, status};
    use crate::factory::{FactoryBody, FactoryRecord, MachineHeld};
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
                factory_versions: [(MachineId::random(), 3)].into(),
            },
            PeerMessage::Hello {
                protocol: PROTOCOL,
                machine: MachineId::random(),
                versions: BTreeMap::new(),
                factory_versions: BTreeMap::new(),
            },
            PeerMessage::FactoryRecords {
                protocol: PROTOCOL,
                records: factory_records(),
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
    fn answers_are_carried_from_the_protocol_after_calls() {
        assert_eq!(ANSWERS_SINCE, CALLS_SINCE + 1);
    }

    #[test]
    fn factory_records_are_carried_from_protocol_5_and_this_build_speaks_it() {
        assert_eq!(FACTORY_SINCE, ANSWERS_SINCE + 1);
        assert_eq!(FACTORY_SINCE, 5);
        assert_eq!(PROTOCOL, UPDATES_SINCE);
    }

    #[test]
    fn update_calls_are_carried_from_protocol_6_the_protocol_after_factory_records() {
        assert_eq!(UPDATES_SINCE, FACTORY_SINCE + 1);
        assert_eq!(UPDATES_SINCE, 6);
    }

    #[test]
    fn a_hello_from_before_protocol_5_reads_with_no_factory_versions() {
        let machine = "67e55044-10b1-426f-9247-bb680e5fe0c8";
        let id = MachineId(machine.parse().unwrap());
        let hello: PeerMessage = serde_json::from_str(&format!(
            r#"{{"type":"hello","protocol":4,"machine":"{machine}","versions":{{"{machine}":4}}}}"#
        ))
        .unwrap();
        assert_eq!(
            hello,
            PeerMessage::Hello {
                protocol: 4,
                machine: id,
                versions: [(id, 4)].into(),
                factory_versions: BTreeMap::new(),
            }
        );
    }

    #[test]
    fn a_hello_carries_its_factory_versions_apart_from_its_record_versions() {
        let me = MachineId::random();
        let hello = PeerMessage::Hello {
            protocol: PROTOCOL,
            machine: me,
            versions: [(me, 9)].into(),
            factory_versions: [(me, 2)].into(),
        };
        let json = serde_json::to_value(&hello).unwrap();
        assert_eq!(json["versions"][me.to_string()], 9, "{json}");
        assert_eq!(json["factory_versions"][me.to_string()], 2, "{json}");
    }

    #[test]
    fn factory_messages_are_written_under_their_names() {
        let records = PeerMessage::FactoryRecords {
            protocol: PROTOCOL,
            records: factory_records(),
        };
        let json = serde_json::to_value(&records).unwrap();
        assert_eq!(json["type"], "factory_records", "{json}");
        assert_eq!(json["records"][0]["kind"], "run_event", "{json}");
        let status = UiMessage::FactoryStatus {
            protocol: PROTOCOL,
            status: status(),
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["type"], "factory_status", "{json}");
        assert_eq!(json["status"]["pass"], 41, "{json}");
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
                online: Some(vec![MachineId::random(), MachineId::random()]),
            },
            UiMessage::State {
                protocol: PROTOCOL,
                records: records(),
                settings: settings(),
                online: None,
            },
            UiMessage::Record {
                protocol: PROTOCOL,
                record: AnyRecord::Session(session()),
            },
            UiMessage::Settings {
                protocol: PROTOCOL,
                settings: settings(),
            },
            UiMessage::FactoryStatus {
                protocol: PROTOCOL,
                status: status(),
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
    fn a_state_carries_the_online_set_and_parses_without_one() {
        let me = MachineId::random();
        let state = UiMessage::State {
            protocol: PROTOCOL,
            records: vec![],
            settings: SharedSettings::default(),
            online: Some(vec![me]),
        };
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(json["online"], serde_json::json!([me]), "{json}");

        let mut without = json.clone();
        without.as_object_mut().unwrap().remove("online");
        assert_eq!(
            serde_json::from_value::<UiMessage>(without).unwrap(),
            UiMessage::State {
                protocol: PROTOCOL,
                records: vec![],
                settings: SharedSettings::default(),
                online: None,
            }
        );

        let none = UiMessage::State {
            protocol: PROTOCOL,
            records: vec![],
            settings: SharedSettings::default(),
            online: None,
        };
        let json = serde_json::to_value(&none).unwrap();
        assert!(json.get("online").is_none(), "{json}");
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
                    factory_versions: BTreeMap::new(),
                },
            ),
            (
                format!(
                    r#"{{"type":"hello","protocol":5,"machine":"{machine}","versions":{{}},"factory_versions":{{"{machine}":6}}}}"#
                ),
                PeerMessage::Hello {
                    protocol: 5,
                    machine: id,
                    versions: BTreeMap::new(),
                    factory_versions: [(id, 6)].into(),
                },
            ),
            (
                format!(
                    r#"{{"type":"factory_records","protocol":5,"records":[{{"kind":"machine_held","owner":"{machine}","version":2,"body":{{"logins":[],"skill":true}}}}]}}"#
                ),
                PeerMessage::FactoryRecords {
                    protocol: 5,
                    records: vec![FactoryRecord {
                        owner: id,
                        version: 2,
                        body: FactoryBody::Held(MachineHeld {
                            logins: vec![],
                            skill: true,
                        }),
                    }],
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
                    online: None,
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

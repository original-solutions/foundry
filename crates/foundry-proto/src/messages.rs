//! Peer and UI messages, each carrying a protocol number.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::MachineId;
    use crate::record::AnyRecord;
    use crate::record::tests::{machine_sample, session, terminal, workspace};
    use crate::settings::tests::settings;
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
        let json = r#"{"type":"records","protocol":7,"records":[]}"#;
        let message: PeerMessage = serde_json::from_str(json).unwrap();
        assert_eq!(message.protocol(), 7);
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

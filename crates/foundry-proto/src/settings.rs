//! Shared settings and their sequence number.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ids::{MachineId, ProjectId};

/// Settings every machine shares, changed only through the acting coordinator.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SharedSettings {
    /// The coordinator's sequence number for this edit. Higher wins.
    pub seq: u64,
    pub machines: Vec<Machine>,
    /// Machines in the order they take the coordinator role, first first.
    pub priority: Vec<MachineId>,
    pub projects: Vec<Project>,
    pub limits: BTreeMap<MachineId, Limits>,
}

/// A machine in the fleet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Machine {
    pub id: MachineId,
    pub name: String,
    /// Its Tailscale address.
    pub address: String,
}

/// A repo and a name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    /// The URL to clone.
    pub repo: String,
}

/// The limits placement keeps a machine under.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    pub max_sessions: u32,
    /// A NaN or infinite value fails to serialize.
    #[serde(serialize_with = "crate::serialize_finite")]
    pub max_load_per_core: f64,
    /// Bytes.
    pub min_free_disk: u64,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{assert_round_trip, parse_error};

    pub(crate) fn settings() -> SharedSettings {
        let mac = MachineId::random();
        let vps = MachineId::random();
        SharedSettings {
            seq: 42,
            machines: vec![
                Machine {
                    id: mac,
                    name: "trilogy-mac".into(),
                    address: "100.64.0.1".into(),
                },
                Machine {
                    id: vps,
                    name: "vps".into(),
                    address: "100.64.0.2".into(),
                },
            ],
            priority: vec![vps, mac],
            projects: vec![Project {
                id: ProjectId::random(),
                name: "foundry".into(),
                repo: "https://github.com/original-solutions/foundry.git".into(),
            }],
            limits: [(
                mac,
                Limits {
                    max_sessions: 6,
                    max_load_per_core: 1.0,
                    min_free_disk: 20 * 1024 * 1024 * 1024,
                },
            )]
            .into(),
            stamps: [
                (SettingKey::Machine(mac), Stamp { seq: 40, by: vps }),
                (SettingKey::Priority, Stamp { seq: 42, by: mac }),
                (
                    SettingKey::Overwritten(Box::new(SettingKey::Limits(vps))),
                    Stamp { seq: 41, by: vps },
                ),
            ]
            .into(),
            seen: [(mac, 42), (vps, 41)].into(),
            overwritten: [(
                SettingKey::Limits(vps),
                Overwritten {
                    kept: None,
                    lost: Some(SettingValue::Limits(Limits {
                        max_sessions: 2,
                        max_load_per_core: 0.5,
                        min_free_disk: 1,
                    })),
                },
            )]
            .into(),
        }
    }

    #[test]
    fn settings_round_trip() {
        assert_round_trip(&settings());
        assert_round_trip(&SharedSettings::default());
    }

    #[test]
    fn limits_are_keyed_by_machine_id() {
        let settings = settings();
        let json = serde_json::to_value(&settings).unwrap();
        let mac = settings.machines[0].id.to_string();
        assert_eq!(json["limits"][&mac]["max_sessions"], 6);
    }

    #[test]
    fn an_unknown_field_is_ignored() {
        let settings = settings();
        let mut json = serde_json::to_value(&settings).unwrap();
        json["added_later"] = serde_json::json!([]);
        json["machines"][0]["added_later"] = serde_json::json!({});
        json["projects"][0]["added_later"] = serde_json::json!(0);
        let mac = settings.machines[0].id.to_string();
        json["limits"][&mac]["added_later"] = serde_json::json!(null);
        let back: SharedSettings = serde_json::from_value(json).unwrap();
        assert_eq!(back, settings);
    }

    #[test]
    fn settings_read_from_their_wire_form() {
        let mac = "67e55044-10b1-426f-9247-bb680e5fe0c8";
        let project = "11111111-1111-4111-8111-111111111111";
        let json = format!(
            r#"{{"seq":3,"machines":[{{"id":"{mac}","name":"m","address":"100.64.0.1"}}],"priority":["{mac}"],"projects":[{{"id":"{project}","name":"p","repo":"r"}}],"limits":{{"{mac}":{{"max_sessions":2,"max_load_per_core":1.5,"min_free_disk":10}}}}}}"#
        );
        let mac = MachineId(mac.parse().unwrap());
        let expected = SharedSettings {
            seq: 3,
            machines: vec![Machine {
                id: mac,
                name: "m".into(),
                address: "100.64.0.1".into(),
            }],
            priority: vec![mac],
            projects: vec![Project {
                id: ProjectId(project.parse().unwrap()),
                name: "p".into(),
                repo: "r".into(),
            }],
            limits: [(
                mac,
                Limits {
                    max_sessions: 2,
                    max_load_per_core: 1.5,
                    min_free_disk: 10,
                },
            )]
            .into(),
            ..SharedSettings::default()
        };
        assert_eq!(
            serde_json::from_str::<SharedSettings>(&json).unwrap(),
            expected
        );
    }

    #[test]
    fn the_older_form_without_stamps_still_reads() {
        let json = r#"{"seq":7,"machines":[],"priority":[],"projects":[],"limits":{}}"#;
        let read: SharedSettings = serde_json::from_str(json).unwrap();
        assert_eq!(
            read,
            SharedSettings {
                seq: 7,
                ..SharedSettings::default()
            }
        );
        assert!(read.stamps.is_empty() && read.seen.is_empty() && read.overwritten.is_empty());
    }

    #[test]
    fn stamped_settings_read_from_their_wire_form() {
        let mac = "67e55044-10b1-426f-9247-bb680e5fe0c8";
        let json = format!(
            r#"{{"seq":5,"machines":[],"priority":[],"projects":[],"limits":{{}},
                "stamps":{{"priority":{{"seq":5,"by":"{mac}"}},"overwritten:limits:{mac}":{{"seq":4,"by":"{mac}"}}}},
                "seen":{{"{mac}":5}},
                "overwritten":{{"limits:{mac}":{{"kept":{{"setting":"priority","value":["{mac}"]}},"lost":null}}}}}}"#
        );
        let id = MachineId(mac.parse().unwrap());
        let read: SharedSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(read.stamps[&SettingKey::Priority], Stamp { seq: 5, by: id });
        assert_eq!(
            read.stamps[&SettingKey::Overwritten(Box::new(SettingKey::Limits(id)))],
            Stamp { seq: 4, by: id }
        );
        assert_eq!(read.seen, [(id, 5)].into());
        assert_eq!(
            read.overwritten[&SettingKey::Limits(id)],
            Overwritten {
                kept: Some(SettingValue::Priority(vec![id])),
                lost: None,
            }
        );
    }

    #[test]
    fn a_setting_key_is_written_as_text_and_read_back() {
        let id = MachineId::random();
        let project = ProjectId::random();
        let keys = [
            (SettingKey::Machine(id), format!("machine:{id}")),
            (SettingKey::Priority, "priority".to_owned()),
            (SettingKey::Project(project), format!("project:{project}")),
            (SettingKey::Limits(id), format!("limits:{id}")),
            (
                SettingKey::Overwritten(Box::new(SettingKey::Project(project))),
                format!("overwritten:project:{project}"),
            ),
            (
                SettingKey::Overwritten(Box::new(SettingKey::Priority)),
                "overwritten:priority".to_owned(),
            ),
        ];
        for (key, text) in keys {
            assert_eq!(key.to_string(), text);
            assert_eq!(text.parse::<SettingKey>().unwrap(), key);
            assert_round_trip(&key);
        }
    }

    #[test]
    fn a_setting_key_that_names_nothing_is_refused() {
        let id = MachineId::random();
        for text in [
            "".to_owned(),
            "gossip".to_owned(),
            "machine:not-a-uuid".to_owned(),
            "priority:x".to_owned(),
            format!("overwritten:overwritten:machine:{id}"),
            "overwritten:".to_owned(),
        ] {
            assert!(text.parse::<SettingKey>().is_err(), "{text}");
            parse_error::<SettingKey>(&format!("\"{text}\""));
        }
    }

    #[test]
    fn a_higher_seq_beats_and_an_equal_seq_goes_to_the_lower_machine() {
        let low = MachineId(uuid::Uuid::from_u128(1));
        let high = MachineId(uuid::Uuid::from_u128(2));
        assert!(Stamp { seq: 3, by: high } > Stamp { seq: 2, by: low });
        assert!(Stamp { seq: 2, by: low } > Stamp { seq: 2, by: high });
        assert!(Stamp { seq: 1, by: high } > Stamp::NONE);
        assert_eq!(Stamp::default(), Stamp::NONE);
    }

    #[test]
    fn each_edit_names_its_setting_and_the_value_it_leaves() {
        let settings = settings();
        let machine = settings.machines[0].clone();
        let project = settings.projects[0].clone();
        let limits = settings.limits[&machine.id].clone();
        let edits = [
            (
                Edit::SetMachine {
                    machine: machine.clone(),
                },
                SettingKey::Machine(machine.id),
                Some(SettingValue::Machine(machine.clone())),
            ),
            (
                Edit::RemoveMachine { id: machine.id },
                SettingKey::Machine(machine.id),
                None,
            ),
            (
                Edit::Priority {
                    order: settings.priority.clone(),
                },
                SettingKey::Priority,
                Some(SettingValue::Priority(settings.priority.clone())),
            ),
            (
                Edit::SetProject {
                    project: project.clone(),
                },
                SettingKey::Project(project.id),
                Some(SettingValue::Project(project.clone())),
            ),
            (
                Edit::RemoveProject { id: project.id },
                SettingKey::Project(project.id),
                None,
            ),
            (
                Edit::SetLimits {
                    machine: machine.id,
                    limits: limits.clone(),
                },
                SettingKey::Limits(machine.id),
                Some(SettingValue::Limits(limits)),
            ),
            (
                Edit::RemoveLimits {
                    machine: machine.id,
                },
                SettingKey::Limits(machine.id),
                None,
            ),
            (
                Edit::Dismiss {
                    setting: SettingKey::Priority,
                },
                SettingKey::Overwritten(Box::new(SettingKey::Priority)),
                None,
            ),
        ];
        for (edit, key, value) in edits {
            assert_eq!(edit.key(), key, "{edit:?}");
            assert_eq!(edit.value(), value, "{edit:?}");
            assert_round_trip(&edit);
        }
    }

    #[test]
    fn an_edit_reads_from_its_wire_form() {
        let mac = "67e55044-10b1-426f-9247-bb680e5fe0c8";
        let id = MachineId(mac.parse().unwrap());
        let cases = [
            (
                format!(r#"{{"kind":"remove_machine","id":"{mac}"}}"#),
                Edit::RemoveMachine { id },
            ),
            (
                format!(r#"{{"kind":"priority","order":["{mac}"]}}"#),
                Edit::Priority { order: vec![id] },
            ),
            (
                format!(r#"{{"kind":"dismiss","setting":"limits:{mac}"}}"#),
                Edit::Dismiss {
                    setting: SettingKey::Limits(id),
                },
            ),
        ];
        for (json, expected) in cases {
            assert_eq!(serde_json::from_str::<Edit>(&json).expect(&json), expected);
        }
    }

    #[test]
    fn a_non_finite_load_limit_cannot_be_written() {
        let mut settings = settings();
        let mac = settings.machines[0].id;
        for load in [f64::NAN, f64::INFINITY] {
            settings.limits.get_mut(&mac).unwrap().max_load_per_core = load;
            assert!(serde_json::to_string(&settings).is_err(), "{load}");
        }
    }
}

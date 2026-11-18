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
    use crate::assert_round_trip;

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
        };
        assert_eq!(
            serde_json::from_str::<SharedSettings>(&json).unwrap(),
            expected
        );
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

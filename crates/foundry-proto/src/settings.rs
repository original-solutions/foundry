//! Shared settings and their sequence number.

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::assert_round_trip;
    use crate::ids::{MachineId, ProjectId};

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
        let back: SharedSettings = serde_json::from_value(json).unwrap();
        assert_eq!(back, settings);
    }
}

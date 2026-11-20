//! Shared settings, their sequence number, and the stamps that say which edit set each one.
//!
//! A setting is one first-level entry: one machine, the priority order, one project, one
//! machine's limits, or one "setting overwritten" item. Each is named by a [`SettingKey`]. The
//! acting coordinator numbers every edit and stamps the setting it changed with a [`Stamp`];
//! a removed setting keeps its stamp, so a merge does not bring it back.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::ids::{MachineId, ProjectId};

/// Settings every machine shares, changed only through the acting coordinator.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SharedSettings {
    /// The highest number this copy holds. A copy with a higher one replaces a held copy.
    pub seq: u64,
    pub machines: Vec<Machine>,
    /// Machines in the order they take the coordinator role, first first.
    pub priority: Vec<MachineId>,
    pub projects: Vec<Project>,
    pub limits: BTreeMap<MachineId, Limits>,
    /// The edit that last set each setting, removed ones included. A setting with no stamp has
    /// [`Stamp::NONE`].
    #[serde(default)]
    pub stamps: BTreeMap<SettingKey, Stamp>,
    /// The highest number each coordinator gave an edit that this copy includes.
    #[serde(default)]
    pub seen: BTreeMap<MachineId, u64>,
    /// "Setting overwritten" items, keyed by the setting two sides of a split both edited.
    #[serde(default)]
    pub overwritten: BTreeMap<SettingKey, Overwritten>,
}

/// One first-level setting. Written as text: `machine:<id>`, `priority`, `project:<id>`,
/// `limits:<id>`, or `overwritten:<key>` for the item about another setting.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SettingKey {
    Machine(MachineId),
    Priority,
    Project(ProjectId),
    Limits(MachineId),
    /// The "setting overwritten" item about a setting that is not itself one.
    Overwritten(Box<SettingKey>),
}

impl SettingKey {
    pub fn is_overwritten(&self) -> bool {
        matches!(self, Self::Overwritten(_))
    }
}

impl fmt::Display for SettingKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Machine(id) => write!(f, "machine:{id}"),
            Self::Priority => f.write_str("priority"),
            Self::Project(id) => write!(f, "project:{id}"),
            Self::Limits(id) => write!(f, "limits:{id}"),
            Self::Overwritten(key) => write!(f, "overwritten:{key}"),
        }
    }
}

impl FromStr for SettingKey {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let refused = || format!("`{text}` names no setting");
        let id = |rest: &str| rest.parse::<uuid::Uuid>().map_err(|_| refused());
        if text == "priority" {
            return Ok(Self::Priority);
        }
        let (kind, rest) = text.split_once(':').ok_or_else(refused)?;
        match kind {
            "machine" => Ok(Self::Machine(MachineId(id(rest)?))),
            "project" => Ok(Self::Project(ProjectId(id(rest)?))),
            "limits" => Ok(Self::Limits(MachineId(id(rest)?))),
            "overwritten" => match rest.parse::<Self>() {
                Ok(inner) if !inner.is_overwritten() => Ok(Self::Overwritten(Box::new(inner))),
                _ => Err(refused()),
            },
            _ => Err(refused()),
        }
    }
}

impl Serialize for SettingKey {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for SettingKey {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// The edit that set a setting: the number the acting coordinator gave it, and that
/// coordinator. Of two stamps the greater wins: the higher `seq`, then on an equal `seq` the
/// lower `by`, so every machine picks the same one whatever order copies reach it in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Stamp {
    pub seq: u64,
    pub by: MachineId,
}

impl Stamp {
    /// The stamp of a setting no numbered edit has set; every numbered edit beats it.
    pub const NONE: Self = Self {
        seq: 0,
        by: MachineId(uuid::Uuid::nil()),
    };
}

impl Default for Stamp {
    fn default() -> Self {
        Self::NONE
    }
}

impl Ord for Stamp {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.seq
            .cmp(&other.seq)
            .then_with(|| other.by.cmp(&self.by))
    }
}

impl PartialOrd for Stamp {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// The value of one setting that is not an overwritten item.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "setting", content = "value", rename_all = "snake_case")]
pub enum SettingValue {
    Machine(Machine),
    Priority(Vec<MachineId>),
    Project(Project),
    Limits(Limits),
}

/// Two sides of a split both edited a setting: the value that holds and the one it replaced.
/// `None` is a removed setting.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Overwritten {
    pub kept: Option<SettingValue>,
    pub lost: Option<SettingValue>,
}

/// A change to one setting, made through the acting coordinator.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Edit {
    SetMachine {
        machine: Machine,
    },
    RemoveMachine {
        id: MachineId,
    },
    Priority {
        order: Vec<MachineId>,
    },
    SetProject {
        project: Project,
    },
    RemoveProject {
        id: ProjectId,
    },
    SetLimits {
        machine: MachineId,
        limits: Limits,
    },
    RemoveLimits {
        machine: MachineId,
    },
    /// Dismisses the "setting overwritten" item about `setting`.
    Dismiss {
        setting: SettingKey,
    },
}

impl Edit {
    /// The setting this edit changes.
    pub fn key(&self) -> SettingKey {
        match self {
            Self::SetMachine { machine } => SettingKey::Machine(machine.id),
            Self::RemoveMachine { id } => SettingKey::Machine(*id),
            Self::Priority { .. } => SettingKey::Priority,
            Self::SetProject { project } => SettingKey::Project(project.id),
            Self::RemoveProject { id } => SettingKey::Project(*id),
            Self::SetLimits { machine, .. } | Self::RemoveLimits { machine } => {
                SettingKey::Limits(*machine)
            }
            Self::Dismiss { setting } => SettingKey::Overwritten(Box::new(setting.clone())),
        }
    }

    /// The value the setting holds after this edit, `None` when the edit removes it.
    pub fn value(&self) -> Option<SettingValue> {
        match self {
            Self::SetMachine { machine } => Some(SettingValue::Machine(machine.clone())),
            Self::Priority { order } => Some(SettingValue::Priority(order.clone())),
            Self::SetProject { project } => Some(SettingValue::Project(project.clone())),
            Self::SetLimits { limits, .. } => Some(SettingValue::Limits(limits.clone())),
            Self::RemoveMachine { .. }
            | Self::RemoveProject { .. }
            | Self::RemoveLimits { .. }
            | Self::Dismiss { .. } => None,
        }
    }
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
                (
                    StampKey::Setting(SettingKey::Machine(mac)),
                    Stamp { seq: 40, by: vps },
                ),
                (
                    StampKey::Setting(SettingKey::Priority),
                    Stamp { seq: 42, by: mac },
                ),
                (
                    StampKey::Overwritten(SettingKey::Limits(vps)),
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
        assert_eq!(
            read.stamps[&StampKey::Setting(SettingKey::Priority)],
            Stamp { seq: 5, by: id }
        );
        assert_eq!(
            read.stamps[&StampKey::Overwritten(SettingKey::Limits(id))],
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
        ];
        for (key, text) in keys {
            assert_eq!(key.to_string(), text);
            assert_eq!(text.parse::<SettingKey>().unwrap(), key);
            assert_round_trip(&key);
            let stamp_key = StampKey::Setting(key.clone());
            assert_eq!(stamp_key.to_string(), text);
            assert_eq!(text.parse::<StampKey>().unwrap(), stamp_key);
            assert_round_trip(&stamp_key);
            let item = StampKey::Overwritten(key);
            let text = format!("overwritten:{text}");
            assert_eq!(item.to_string(), text);
            assert_eq!(text.parse::<StampKey>().unwrap(), item);
            assert_round_trip(&item);
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
            assert!(text.parse::<StampKey>().is_err(), "{text}");
            parse_error::<StampKey>(&format!("\"{text}\""));
        }
        // An item is not itself a setting: no edit can name one as its setting.
        let item = format!("overwritten:machine:{id}");
        assert!(item.parse::<SettingKey>().is_err(), "{item}");
        parse_error::<Edit>(&format!(r#"{{"kind":"dismiss","setting":"{item}"}}"#));
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
                StampKey::Setting(SettingKey::Machine(machine.id)),
                Some(SettingValue::Machine(machine.clone())),
            ),
            (
                Edit::RemoveMachine { id: machine.id },
                StampKey::Setting(SettingKey::Machine(machine.id)),
                None,
            ),
            (
                Edit::Priority {
                    order: settings.priority.clone(),
                },
                StampKey::Setting(SettingKey::Priority),
                Some(SettingValue::Priority(settings.priority.clone())),
            ),
            (
                Edit::SetProject {
                    project: project.clone(),
                },
                StampKey::Setting(SettingKey::Project(project.id)),
                Some(SettingValue::Project(project.clone())),
            ),
            (
                Edit::RemoveProject { id: project.id },
                StampKey::Setting(SettingKey::Project(project.id)),
                None,
            ),
            (
                Edit::SetLimits {
                    machine: machine.id,
                    limits: limits.clone(),
                },
                StampKey::Setting(SettingKey::Limits(machine.id)),
                Some(SettingValue::Limits(limits)),
            ),
            (
                Edit::RemoveLimits {
                    machine: machine.id,
                },
                StampKey::Setting(SettingKey::Limits(machine.id)),
                None,
            ),
            (
                Edit::Dismiss {
                    setting: SettingKey::Priority,
                },
                StampKey::Overwritten(SettingKey::Priority),
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

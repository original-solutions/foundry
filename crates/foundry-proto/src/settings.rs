//! Shared settings, their sequence number, and the stamps that say which edit set each one.
//!
//! A setting is one first-level entry: one machine, the priority order, one project, one
//! machine's limits, the fleet's factory settings, one project's factory setting, each named by
//! a [`SettingKey`], or the "setting overwritten" item about one of those. The acting coordinator numbers every edit and stamps what it changed with a
//! [`Stamp`], kept under a [`StampKey`]; a removed setting keeps its stamp, so a merge does not
//! bring it back.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::factory::{FactorySettings, ProjectFactory};
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
    pub stamps: BTreeMap<StampKey, Stamp>,
    /// The highest number each coordinator gave an edit that this copy includes.
    #[serde(default)]
    pub seen: BTreeMap<MachineId, u64>,
    /// "Setting overwritten" items, keyed by the setting two sides of a split both edited.
    #[serde(default)]
    pub overwritten: BTreeMap<SettingKey, Overwritten>,
    /// Edits recently numbered, by the id they were sent under, so an edit sent again is not
    /// numbered twice and its sender can find its stamp in a copy.
    #[serde(default)]
    pub numbered: BTreeMap<EditId, Stamp>,
    /// The fleet's factory settings, once set. Never sent to a peer before
    /// [`FACTORY_SINCE`](crate::messages::FACTORY_SINCE), and not written when unset, so a copy
    /// without them is written as it was before them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub factory: Option<FactorySettings>,
    /// Each project's factory setting, by project.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub project_factory: BTreeMap<ProjectId, ProjectFactory>,
}

impl SharedSettings {
    /// The project named `name`, compared trimmed and in any case: the one rule for when two
    /// project names are the same.
    pub fn project_named(&self, name: &str) -> Option<&Project> {
        let wanted = name.trim().to_lowercase();
        self.projects
            .iter()
            .find(|p| p.name.trim().to_lowercase() == wanted)
    }

    /// A copy for a peer that speaks `protocol`. A peer before
    /// [`FACTORY_SINCE`](crate::messages::FACTORY_SINCE) refuses a setting key it does not
    /// know, so its copy has no factory setting, no stamp of one or of its item, and no
    /// "setting overwritten" item about one. Every other setting is as it is here.
    pub fn for_protocol(&self, protocol: u16) -> SharedSettings {
        let mut copy = self.clone();
        if protocol < crate::messages::FACTORY_SINCE {
            copy.factory = None;
            copy.project_factory.clear();
            copy.stamps.retain(|key, _| !key.setting().is_factory());
            copy.overwritten.retain(|key, _| !key.is_factory());
        }
        copy
    }
}

/// The id a machine gives an edit it sends to the acting coordinator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EditId(pub uuid::Uuid);

impl EditId {
    /// A new random id.
    pub fn random() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl fmt::Display for EditId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// What the acting coordinator did with an edit sent to it. A refusal's reason carries no
/// setting value and no secret.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    /// The edit was numbered; this is its stamp.
    Numbered { stamp: Stamp },
    /// The edit was not applied, and why. `conflict` says it breaks a rule of the settings as
    /// the acting machine holds them, such as a second project of one name. It is written only
    /// when true, and a machine that does not know it reads the refusal as any other.
    Refused {
        reason: String,
        #[serde(default, skip_serializing_if = "is_false")]
        conflict: bool,
    },
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// One first-level setting. Written as text: `machine:<id>`, `priority`, `project:<id>`,
/// `limits:<id>`, `factory` or `factory:<project id>`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SettingKey {
    Machine(MachineId),
    Priority,
    Project(ProjectId),
    Limits(MachineId),
    /// The fleet's factory settings.
    Factory,
    /// One project's factory setting.
    ProjectFactory(ProjectId),
}

impl SettingKey {
    /// True for a factory key, which a peer before
    /// [`FACTORY_SINCE`](crate::messages::FACTORY_SINCE) cannot read.
    pub fn is_factory(&self) -> bool {
        matches!(self, Self::Factory | Self::ProjectFactory(_))
    }
}

/// What a stamp is kept for: a setting, or the "setting overwritten" item about one. Written
/// as the setting's key, or `overwritten:<key>`. An item is never about another item.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StampKey {
    Setting(SettingKey),
    Overwritten(SettingKey),
}

impl StampKey {
    /// The setting this stamp is for, or the one its item is about.
    pub fn setting(&self) -> &SettingKey {
        match self {
            Self::Setting(key) | Self::Overwritten(key) => key,
        }
    }
}

impl fmt::Display for SettingKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Machine(id) => write!(f, "machine:{id}"),
            Self::Priority => f.write_str("priority"),
            Self::Project(id) => write!(f, "project:{id}"),
            Self::Limits(id) => write!(f, "limits:{id}"),
            Self::Factory => f.write_str("factory"),
            Self::ProjectFactory(id) => write!(f, "factory:{id}"),
        }
    }
}

impl FromStr for SettingKey {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let refused = || format!("`{text}` names no setting");
        let id = |rest: &str| rest.parse::<uuid::Uuid>().map_err(|_| refused());
        match text {
            "priority" => return Ok(Self::Priority),
            "factory" => return Ok(Self::Factory),
            _ => {}
        }
        let (kind, rest) = text.split_once(':').ok_or_else(refused)?;
        match kind {
            "machine" => Ok(Self::Machine(MachineId(id(rest)?))),
            "project" => Ok(Self::Project(ProjectId(id(rest)?))),
            "limits" => Ok(Self::Limits(MachineId(id(rest)?))),
            "factory" => Ok(Self::ProjectFactory(ProjectId(id(rest)?))),
            _ => Err(refused()),
        }
    }
}

impl fmt::Display for StampKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Setting(key) => key.fmt(f),
            Self::Overwritten(key) => write!(f, "overwritten:{key}"),
        }
    }
}

impl FromStr for StampKey {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text.strip_prefix("overwritten:") {
            Some(rest) => rest.parse().map(Self::Overwritten),
            None => text.parse().map(Self::Setting),
        }
    }
}

/// Serde for a key as its text, so it can key a JSON object.
macro_rules! as_text {
    ($key:ty) => {
        impl Serialize for $key {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $key {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                String::deserialize(d)?
                    .parse()
                    .map_err(serde::de::Error::custom)
            }
        }
    };
}

as_text!(SettingKey);
as_text!(StampKey);

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
    Factory(FactorySettings),
    ProjectFactory(ProjectFactory),
}

/// Two sides of a split both edited a setting: the value that holds and the one it replaced.
/// `None` is a removed setting.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Overwritten {
    pub kept: Option<SettingValue>,
    pub lost: Option<SettingValue>,
    /// The edit that set `lost`. When more than two sides edited the setting, the item keeps
    /// the latest edit replaced, so every machine shows the same one.
    pub lost_stamp: Stamp,
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
    /// Dismisses the "setting overwritten" item about `setting`: the key of the setting both
    /// sides edited, as the item is keyed in [`SharedSettings::overwritten`].
    Dismiss {
        setting: SettingKey,
    },
    /// Sets the fleet's factory settings.
    SetFactory {
        settings: FactorySettings,
    },
    /// Sets one project's factory setting.
    SetProjectFactory {
        project: ProjectId,
        factory: ProjectFactory,
    },
}

impl Edit {
    /// The setting, or the item, this edit changes.
    pub fn key(&self) -> StampKey {
        let setting = match self {
            Self::SetMachine { machine } => SettingKey::Machine(machine.id),
            Self::RemoveMachine { id } => SettingKey::Machine(*id),
            Self::Priority { .. } => SettingKey::Priority,
            Self::SetProject { project } => SettingKey::Project(project.id),
            Self::RemoveProject { id } => SettingKey::Project(*id),
            Self::SetLimits { machine, .. } | Self::RemoveLimits { machine } => {
                SettingKey::Limits(*machine)
            }
            Self::Dismiss { setting } => return StampKey::Overwritten(setting.clone()),
            Self::SetFactory { .. } => SettingKey::Factory,
            Self::SetProjectFactory { project, .. } => SettingKey::ProjectFactory(*project),
        };
        StampKey::Setting(setting)
    }

    /// The value the setting holds after this edit, `None` when the edit removes it.
    pub fn value(&self) -> Option<SettingValue> {
        match self {
            Self::SetMachine { machine } => Some(SettingValue::Machine(machine.clone())),
            Self::Priority { order } => Some(SettingValue::Priority(order.clone())),
            Self::SetProject { project } => Some(SettingValue::Project(project.clone())),
            Self::SetLimits { limits, .. } => Some(SettingValue::Limits(limits.clone())),
            Self::SetFactory { settings } => Some(SettingValue::Factory(settings.clone())),
            Self::SetProjectFactory { factory, .. } => {
                Some(SettingValue::ProjectFactory(factory.clone()))
            }
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
    /// Its Ed25519 public key, as hex, so every machine admits it. Empty in an entry from a
    /// machine before keys were listed.
    #[serde(default)]
    pub key: String,
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
                    key: String::new(),
                },
                Machine {
                    id: vps,
                    name: "vps".into(),
                    address: "100.64.0.2".into(),
                    key: String::new(),
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
                    lost_stamp: Stamp { seq: 40, by: vps },
                },
            )]
            .into(),
            numbered: [(EditId::random(), Stamp { seq: 42, by: mac })].into(),
            factory: None,
            project_factory: BTreeMap::new(),
        }
    }

    #[test]
    fn a_project_is_found_by_its_name_trimmed_and_in_any_case() {
        let mut settings = settings();
        settings.projects[0].name = " Foundry ".into();
        let id = settings.projects[0].id;
        for name in ["foundry", "  FOUNDRY", "fOuNdRy\t"] {
            assert_eq!(
                settings.project_named(name).map(|p| p.id),
                Some(id),
                "{name:?}"
            );
        }
        for name in ["found", "foundry2", "", "foun dry"] {
            assert_eq!(settings.project_named(name), None, "{name:?}");
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
                key: String::new(),
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
    fn a_machine_carries_its_public_key_through_the_json_round_trip() {
        let machine = Machine {
            id: MachineId::random(),
            name: "personal-mac".into(),
            address: "100.91.54.18".into(),
            key: "ab".repeat(32),
        };
        assert_round_trip(&machine);
        let json = serde_json::to_value(&machine).unwrap();
        assert_eq!(json["key"], "ab".repeat(32));
    }

    #[test]
    fn a_machine_from_an_older_machine_reads_with_an_empty_key() {
        let mac = "67e55044-10b1-426f-9247-bb680e5fe0c8";
        let json = format!(r#"{{"id":"{mac}","name":"m","address":"100.64.0.1"}}"#);
        let read: Machine = serde_json::from_str(&json).unwrap();
        assert_eq!(
            read,
            Machine {
                id: MachineId(mac.parse().unwrap()),
                name: "m".into(),
                address: "100.64.0.1".into(),
                key: String::new(),
            }
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
        assert!(read.numbered.is_empty());
    }

    #[test]
    fn stamped_settings_read_from_their_wire_form() {
        let mac = "67e55044-10b1-426f-9247-bb680e5fe0c8";
        let edit = "22222222-2222-4222-8222-222222222222";
        let json = format!(
            r#"{{"seq":5,"machines":[],"priority":[],"projects":[],"limits":{{}},
                "stamps":{{"priority":{{"seq":5,"by":"{mac}"}},"overwritten:limits:{mac}":{{"seq":4,"by":"{mac}"}}}},
                "seen":{{"{mac}":5}},
                "numbered":{{"{edit}":{{"seq":5,"by":"{mac}"}}}},
                "overwritten":{{"limits:{mac}":{{"kept":{{"setting":"priority","value":["{mac}"]}},"lost":null,"lost_stamp":{{"seq":3,"by":"{mac}"}}}}}}}}"#
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
            read.numbered,
            [(EditId(edit.parse().unwrap()), Stamp { seq: 5, by: id })].into()
        );
        assert_eq!(
            read.overwritten[&SettingKey::Limits(id)],
            Overwritten {
                kept: Some(SettingValue::Priority(vec![id])),
                lost: None,
                lost_stamp: Stamp { seq: 3, by: id },
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
    fn an_edit_id_and_each_outcome_round_trip_and_read_from_their_wire_form() {
        let edit = EditId::random();
        assert_round_trip(&edit);
        assert_eq!(serde_json::to_value(edit).unwrap(), edit.to_string());
        assert_ne!(EditId::random(), edit);

        let mac = "67e55044-10b1-426f-9247-bb680e5fe0c8";
        let by = MachineId(mac.parse().unwrap());
        let cases = [
            (
                format!(r#"{{"kind":"numbered","stamp":{{"seq":9,"by":"{mac}"}}}}"#),
                Outcome::Numbered {
                    stamp: Stamp { seq: 9, by },
                },
            ),
            (
                r#"{"kind":"refused","reason":"it could not be stored"}"#.to_owned(),
                Outcome::Refused {
                    reason: "it could not be stored".into(),
                    conflict: false,
                },
            ),
            (
                r#"{"kind":"refused","reason":"another has that name","conflict":true}"#.to_owned(),
                Outcome::Refused {
                    reason: "another has that name".into(),
                    conflict: true,
                },
            ),
        ];
        for (json, expected) in cases {
            assert_round_trip(&expected);
            assert_eq!(
                serde_json::from_str::<Outcome>(&json).expect(&json),
                expected
            );
            // Written as it is read, so a machine before `conflict` reads every refusal.
            assert_eq!(serde_json::to_string(&expected).unwrap(), json);
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

    // Factory settings (P2, protocol 5).

    pub(crate) fn factory_settings() -> FactorySettings {
        FactorySettings {
            paused: true,
            poll_seconds: 15,
            feedback_seconds: 300,
            max_runs: 4,
            max_gates: 1,
            model: "sonnet".into(),
            effort: "medium".into(),
        }
    }

    pub(crate) fn project_factory() -> ProjectFactory {
        ProjectFactory {
            on: true,
            login: "rawphp".into(),
            paused: false,
        }
    }

    /// [`settings`] with both factory keys set, stamped and overwritten.
    pub(crate) fn settings_with_factory() -> SharedSettings {
        let mut settings = settings();
        let project = settings.projects[0].id;
        let mac = settings.machines[0].id;
        settings.factory = Some(factory_settings());
        settings.project_factory.insert(project, project_factory());
        settings.stamps.insert(
            StampKey::Setting(SettingKey::Factory),
            Stamp { seq: 43, by: mac },
        );
        settings.stamps.insert(
            StampKey::Setting(SettingKey::ProjectFactory(project)),
            Stamp { seq: 44, by: mac },
        );
        settings.stamps.insert(
            StampKey::Overwritten(SettingKey::Factory),
            Stamp { seq: 45, by: mac },
        );
        settings.stamps.insert(
            StampKey::Overwritten(SettingKey::ProjectFactory(project)),
            Stamp { seq: 46, by: mac },
        );
        settings.overwritten.insert(
            SettingKey::Factory,
            Overwritten {
                kept: Some(SettingValue::Factory(factory_settings())),
                lost: Some(SettingValue::Factory(FactorySettings::default())),
                lost_stamp: Stamp { seq: 42, by: mac },
            },
        );
        settings.overwritten.insert(
            SettingKey::ProjectFactory(project),
            Overwritten {
                kept: None,
                lost: Some(SettingValue::ProjectFactory(project_factory())),
                lost_stamp: Stamp { seq: 41, by: mac },
            },
        );
        settings.seq = 46;
        settings
    }

    #[test]
    fn factory_settings_have_the_spec_defaults() {
        let defaults = FactorySettings::default();
        assert_eq!(
            defaults,
            FactorySettings {
                paused: false,
                poll_seconds: 30,
                feedback_seconds: 600,
                max_runs: 3,
                max_gates: 2,
                model: "opus".into(),
                effort: "high".into(),
            }
        );
        assert_eq!(
            serde_json::from_str::<FactorySettings>("{}").unwrap(),
            defaults
        );
        assert_eq!(
            serde_json::from_str::<FactorySettings>(r#"{"max_runs":5}"#).unwrap(),
            FactorySettings {
                max_runs: 5,
                ..defaults
            }
        );
        assert_eq!(
            ProjectFactory::default(),
            ProjectFactory {
                on: false,
                login: String::new(),
                paused: false,
            }
        );
        assert_eq!(
            serde_json::from_str::<ProjectFactory>("{}").unwrap(),
            ProjectFactory::default()
        );
    }

    #[test]
    fn factory_settings_round_trip() {
        assert_round_trip(&factory_settings());
        assert_round_trip(&FactorySettings::default());
        assert_round_trip(&project_factory());
        assert_round_trip(&settings_with_factory());
        let json = serde_json::to_value(factory_settings()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "paused": true,
                "poll_seconds": 15,
                "feedback_seconds": 300,
                "max_runs": 4,
                "max_gates": 1,
                "model": "sonnet",
                "effort": "medium",
            })
        );
    }

    #[test]
    fn poll_seconds_outside_10_to_60_does_not_parse() {
        for seconds in [0, 1, 9, 61, 600, u64::from(u32::MAX) + 1] {
            let error = parse_error::<FactorySettings>(&format!(r#"{{"poll_seconds":{seconds}}}"#));
            assert!(error.contains("poll_seconds"), "{seconds}: {error}");
            let mut json = serde_json::to_value(settings_with_factory()).unwrap();
            json["factory"]["poll_seconds"] = serde_json::json!(seconds);
            assert!(
                serde_json::from_value::<SharedSettings>(json).is_err(),
                "{seconds}"
            );
        }
        parse_error::<FactorySettings>(r#"{"poll_seconds":-10}"#);
        for seconds in [10, 11, 30, 59, 60] {
            let read: FactorySettings =
                serde_json::from_str(&format!(r#"{{"poll_seconds":{seconds}}}"#)).unwrap();
            assert_eq!(read.poll_seconds, seconds);
        }
    }

    #[test]
    fn poll_seconds_outside_10_to_60_cannot_be_written() {
        for seconds in [0, 9, 61, u32::MAX] {
            let factory = FactorySettings {
                poll_seconds: seconds,
                ..FactorySettings::default()
            };
            let error = serde_json::to_string(&factory).expect_err("written");
            assert!(error.to_string().contains("poll_seconds"), "{error}");
            let mut settings = settings_with_factory();
            settings.factory = Some(factory.clone());
            assert!(serde_json::to_string(&settings).is_err(), "{seconds}");
            assert!(
                serde_json::to_string(&Edit::SetFactory { settings: factory }).is_err(),
                "{seconds}"
            );
        }
        for seconds in [10, 60] {
            assert_round_trip(&FactorySettings {
                poll_seconds: seconds,
                ..FactorySettings::default()
            });
        }
    }

    #[test]
    fn shared_settings_hold_the_factory_keys_by_name() {
        let settings = settings_with_factory();
        let project = settings.projects[0].id;
        let json = serde_json::to_value(&settings).unwrap();
        assert_eq!(json["factory"]["max_runs"], 4, "{json}");
        assert_eq!(
            json["project_factory"][project.to_string()]["login"],
            "rawphp"
        );
        assert_eq!(json["stamps"]["factory"]["seq"], 43);
        assert_eq!(json["stamps"][format!("factory:{project}")]["seq"], 44);
        assert_eq!(json["stamps"]["overwritten:factory"]["seq"], 45);
        assert_eq!(json["overwritten"]["factory"]["kept"]["setting"], "factory");
        assert_eq!(
            json["overwritten"][format!("factory:{project}")]["lost"]["setting"],
            "project_factory"
        );
    }

    #[test]
    fn settings_written_before_the_factory_still_parse_and_are_written_as_before() {
        let mac = "67e55044-10b1-426f-9247-bb680e5fe0c8";
        let json = format!(
            r#"{{"seq":5,"machines":[],"priority":["{mac}"],"projects":[],"limits":{{}},
                "stamps":{{"priority":{{"seq":5,"by":"{mac}"}}}},
                "seen":{{"{mac}":5}},
                "overwritten":{{"priority":{{"kept":null,"lost":{{"setting":"priority","value":[]}},"lost_stamp":{{"seq":3,"by":"{mac}"}}}}}}}}"#
        );
        let read: SharedSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(read.factory, None);
        assert!(read.project_factory.is_empty());
        assert_eq!(read.seq, 5);
        let written = serde_json::to_value(&read).unwrap();
        assert!(written.get("factory").is_none(), "{written}");
        assert!(written.get("project_factory").is_none(), "{written}");
        let old = serde_json::to_value(SharedSettings::default()).unwrap();
        assert!(old.get("factory").is_none() && old.get("project_factory").is_none());
    }

    #[test]
    fn a_factory_setting_key_is_written_as_text_and_read_back() {
        let project = ProjectId::random();
        let keys = [
            (SettingKey::Factory, "factory".to_owned()),
            (
                SettingKey::ProjectFactory(project),
                format!("factory:{project}"),
            ),
        ];
        for (key, text) in keys {
            assert_eq!(key.to_string(), text);
            assert_eq!(text.parse::<SettingKey>().unwrap(), key);
            assert_round_trip(&key);
            assert!(key.is_factory(), "{key}");
            let stamp_key = StampKey::Setting(key.clone());
            assert_eq!(text.parse::<StampKey>().unwrap(), stamp_key);
            assert_round_trip(&stamp_key);
            let item = StampKey::Overwritten(key);
            assert_eq!(
                format!("overwritten:{text}").parse::<StampKey>().unwrap(),
                item
            );
            assert_round_trip(&item);
        }
        for key in [
            SettingKey::Machine(MachineId::random()),
            SettingKey::Priority,
            SettingKey::Project(project),
            SettingKey::Limits(MachineId::random()),
        ] {
            assert!(!key.is_factory(), "{key}");
        }
        for text in ["factory:", "factory:not-a-uuid", "factory:x:y", "factories"] {
            assert!(text.parse::<SettingKey>().is_err(), "{text}");
            parse_error::<SettingKey>(&format!("\"{text}\""));
        }
    }

    #[test]
    fn each_factory_edit_names_its_setting_and_the_value_it_leaves() {
        let project = ProjectId::random();
        let edits = [
            (
                Edit::SetFactory {
                    settings: factory_settings(),
                },
                StampKey::Setting(SettingKey::Factory),
                Some(SettingValue::Factory(factory_settings())),
            ),
            (
                Edit::SetProjectFactory {
                    project,
                    factory: project_factory(),
                },
                StampKey::Setting(SettingKey::ProjectFactory(project)),
                Some(SettingValue::ProjectFactory(project_factory())),
            ),
        ];
        for (edit, key, value) in edits {
            assert_eq!(edit.key(), key, "{edit:?}");
            assert_eq!(edit.value(), value, "{edit:?}");
            assert_round_trip(&edit);
        }
        let json = format!(
            r#"{{"kind":"set_project_factory","project":"{project}","factory":{{"on":true,"login":"rawphp","paused":false}}}}"#
        );
        assert_eq!(
            serde_json::from_str::<Edit>(&json).unwrap(),
            Edit::SetProjectFactory {
                project,
                factory: project_factory(),
            }
        );
        parse_error::<Edit>(r#"{"kind":"set_factory","settings":{"poll_seconds":5}}"#);
    }

    #[test]
    fn a_copy_for_protocol_5_or_later_is_the_whole_copy() {
        let settings = settings_with_factory();
        assert_eq!(settings.for_protocol(5), settings);
        assert_eq!(settings.for_protocol(6), settings);
    }

    #[test]
    fn a_copy_for_protocol_4_drops_the_factory_keys_their_stamps_and_their_items() {
        let settings = settings_with_factory();
        for protocol in [1, 2, 3, 4] {
            let copy = settings.for_protocol(protocol);
            assert_eq!(copy.factory, None);
            assert!(copy.project_factory.is_empty());
            assert!(
                copy.stamps.keys().all(|key| !match key {
                    StampKey::Setting(key) | StampKey::Overwritten(key) => key.is_factory(),
                }),
                "{:?}",
                copy.stamps
            );
            assert!(copy.overwritten.keys().all(|key| !key.is_factory()));
            // Everything else is as it was.
            assert_eq!(copy, without_factory(&settings), "protocol {protocol}");
        }
        // Without factory keys a copy loses nothing.
        let plain = super::tests::settings();
        assert_eq!(plain.for_protocol(4), plain);

        fn without_factory(settings: &SharedSettings) -> SharedSettings {
            let mut settings = settings.clone();
            settings.factory = None;
            settings.project_factory.clear();
            settings.stamps.retain(|key, _| {
                !matches!(
                    key,
                    StampKey::Setting(SettingKey::Factory | SettingKey::ProjectFactory(_))
                        | StampKey::Overwritten(
                            SettingKey::Factory | SettingKey::ProjectFactory(_)
                        )
                )
            });
            settings.overwritten.retain(|key, _| {
                !matches!(key, SettingKey::Factory | SettingKey::ProjectFactory(_))
            });
            settings
        }
    }

    /// [`settings_with_factory`] with a factory `seen` that differs from `seen`.
    fn settings_with_factory_seen() -> SharedSettings {
        let mut settings = settings_with_factory();
        let (mac, vps) = (settings.machines[0].id, settings.machines[1].id);
        settings.factory_seen = [(mac, 46), (vps, 12)].into();
        settings
    }

    #[test]
    fn the_factory_seen_round_trips_and_is_written_by_name() {
        let settings = settings_with_factory_seen();
        assert_round_trip(&settings);
        let (mac, vps) = (settings.machines[0].id, settings.machines[1].id);
        let json = serde_json::to_value(&settings).unwrap();
        assert_eq!(json["factory_seen"][mac.to_string()], 46, "{json}");
        assert_eq!(json["factory_seen"][vps.to_string()], 12, "{json}");
        // It is a map of its own: `seen` is as it was.
        assert_eq!(json["seen"][vps.to_string()], 41, "{json}");
    }

    #[test]
    fn a_copy_without_the_factory_seen_parses_with_it_empty_and_is_written_without_it() {
        let mac = "67e55044-10b1-426f-9247-bb680e5fe0c8";
        let json = format!(
            r#"{{"seq":5,"machines":[],"priority":[],"projects":[],"limits":{{}},
                "stamps":{{"factory":{{"seq":5,"by":"{mac}"}}}},
                "seen":{{"{mac}":5}},
                "factory":{{"max_runs":4}}}}"#
        );
        let read: SharedSettings = serde_json::from_str(&json).unwrap();
        assert!(read.factory_seen.is_empty(), "{:?}", read.factory_seen);
        assert_eq!(read.seen, [(MachineId(mac.parse().unwrap()), 5)].into());
        let written = serde_json::to_value(&read).unwrap();
        assert!(written.get("factory_seen").is_none(), "{written}");
        let plain = serde_json::to_value(SharedSettings::default()).unwrap();
        assert!(plain.get("factory_seen").is_none(), "{plain}");
    }

    #[test]
    fn a_copy_for_protocol_4_leaves_the_factory_seen_out_and_one_for_5_keeps_it() {
        let settings = settings_with_factory_seen();
        assert_eq!(settings.for_protocol(5), settings);
        assert_eq!(settings.for_protocol(6), settings);
        for protocol in [1, 2, 3, 4] {
            let copy = settings.for_protocol(protocol);
            assert!(copy.factory_seen.is_empty(), "protocol {protocol}");
            // `seen` is every setting's, so it stays.
            assert_eq!(copy.seen, settings.seen, "protocol {protocol}");
            let json = serde_json::to_value(&copy).unwrap();
            assert!(json.get("factory_seen").is_none(), "{json}");
            let text = json.to_string();
            serde_json::from_str::<v4::SharedSettings>(&text).expect(&text);
        }
    }

    #[test]
    fn a_copy_for_protocol_4_parses_with_the_protocol_4_settings_parser() {
        let settings = settings_with_factory();
        // The whole copy does not: protocol 4 refuses a key it does not know.
        let whole = serde_json::to_string(&settings).unwrap();
        assert!(serde_json::from_str::<v4::SharedSettings>(&whole).is_err());

        let copy = serde_json::to_string(&settings.for_protocol(4)).unwrap();
        let read: v4::SharedSettings = serde_json::from_str(&copy).expect(&copy);
        assert_eq!(read.seq, settings.seq);
        assert_eq!(read.machines, settings.machines);
        assert_eq!(read.priority, settings.priority);
        assert_eq!(read.projects, settings.projects);
        assert_eq!(read.limits, settings.limits);
        assert_eq!(read.seen, settings.seen);
        assert_eq!(read.numbered, settings.numbered);
        assert_eq!(read.stamps.len(), 3);
        assert_eq!(read.overwritten.len(), 1);
    }

    /// The shared settings parser as it stood at `PROTOCOL = 4`, copied, so a copy made for a
    /// peer on protocol 4 is checked against what that peer runs.
    mod v4 {
        // Copied as it stood: the test reads only some of what it parses.
        #![allow(dead_code)]

        use std::collections::BTreeMap;
        use std::str::FromStr;

        use serde::{Deserialize, Deserializer};

        use crate::ids::{MachineId, ProjectId};
        use crate::settings::{EditId, Limits, Machine, Project, Stamp};

        #[derive(Debug, Deserialize)]
        pub struct SharedSettings {
            pub seq: u64,
            pub machines: Vec<Machine>,
            pub priority: Vec<MachineId>,
            pub projects: Vec<Project>,
            pub limits: BTreeMap<MachineId, Limits>,
            #[serde(default)]
            pub stamps: BTreeMap<StampKey, Stamp>,
            #[serde(default)]
            pub seen: BTreeMap<MachineId, u64>,
            #[serde(default)]
            pub overwritten: BTreeMap<SettingKey, Overwritten>,
            #[serde(default)]
            pub numbered: BTreeMap<EditId, Stamp>,
        }

        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum SettingKey {
            Machine(MachineId),
            Priority,
            Project(ProjectId),
            Limits(MachineId),
        }

        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum StampKey {
            Setting(SettingKey),
            Overwritten(SettingKey),
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
                    _ => Err(refused()),
                }
            }
        }

        impl FromStr for StampKey {
            type Err = String;

            fn from_str(text: &str) -> Result<Self, Self::Err> {
                match text.strip_prefix("overwritten:") {
                    Some(rest) => rest.parse().map(Self::Overwritten),
                    None => text.parse().map(Self::Setting),
                }
            }
        }

        macro_rules! from_text {
            ($key:ty) => {
                impl<'de> Deserialize<'de> for $key {
                    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                        String::deserialize(d)?
                            .parse()
                            .map_err(serde::de::Error::custom)
                    }
                }
            };
        }

        from_text!(SettingKey);
        from_text!(StampKey);

        #[derive(Debug, Deserialize)]
        #[serde(tag = "setting", content = "value", rename_all = "snake_case")]
        pub enum SettingValue {
            Machine(Machine),
            Priority(Vec<MachineId>),
            Project(Project),
            Limits(Limits),
        }

        #[derive(Debug, Deserialize)]
        pub struct Overwritten {
            pub kept: Option<SettingValue>,
            pub lost: Option<SettingValue>,
            pub lost_stamp: Stamp,
        }
    }
}

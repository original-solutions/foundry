//! Records with an owner and a version, and their bodies.

use serde::{Deserialize, Serialize};

use crate::ids::{MachineId, ProjectId, SessionId, TerminalId, WorkspaceId};
use crate::state::{SessionState, TerminalState};

/// A body written only by its owner machine, under a version that only goes up.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record<T> {
    /// The machine that writes this record. No other machine may.
    pub owner: MachineId,
    /// The owner's version for this write. Records are ordered by this, never by time.
    pub version: u64,
    /// True once the record is removed, such as an archived workspace.
    pub deleted: bool,
    pub body: T,
}

impl<T> Record<T> {
    /// True only when `self` comes from the same owner as `other` with a higher version.
    ///
    /// Versions count every write by an owner, so the caller must already know both are
    /// copies of the same record: the same kind and body id, or for a machine sample, the
    /// same owner.
    pub fn supersedes(&self, other: &Self) -> bool {
        self.owner == other.owner && self.version > other.version
    }
}

/// A git worktree and branch on its owner machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub project: ProjectId,
    /// The name as typed.
    pub name: String,
    /// The branch made from the name.
    pub branch: String,
    /// The branch the workspace was made from.
    pub base: String,
    /// The terminal its setup commands run in, once started. `None` for a workspace with no
    /// setup, and in a record from a peer that does not write it.
    #[serde(default)]
    pub setup: Option<TerminalId>,
}

/// A PTY in a workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Terminal {
    pub id: TerminalId,
    pub workspace: WorkspaceId,
    /// The owner never writes `Unreachable`. Only a receiver shows it, while the owner is
    /// offline.
    pub state: TerminalState,
    /// The child's exit code once it has exited, when known.
    pub exit_code: Option<i32>,
}

/// Claude Code running in a terminal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub workspace: WorkspaceId,
    pub terminal: TerminalId,
    pub model: String,
    /// The owner never writes `Unreachable`. Only a receiver shows it, while the owner is
    /// offline.
    pub state: SessionState,
    /// The name a person gave it, shown in place of its first prompt. `None` until it is
    /// renamed, once the name is cleared, and in a record from a peer that does not write it.
    #[serde(default)]
    pub name: Option<String>,
}

/// The owner machine's load, memory, disk and session count.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MachineSample {
    /// The 1-minute load divided by logical cores. A NaN or infinite value fails to serialize.
    #[serde(serialize_with = "crate::serialize_finite")]
    pub load_per_core: f64,
    /// Bytes.
    pub free_memory: u64,
    /// Bytes free on the volume holding the data directory.
    pub free_disk: u64,
    pub sessions: u32,
    /// The tag this machine's updater refused after rolling it back, like `v0.2.0`, while the
    /// version installed is not above it. `None` in a sample from a peer that does not write it.
    #[serde(default)]
    pub update_refused: Option<String>,
    /// The version the daemon that wrote this sample was built as, like `0.9.0`. `None` in a
    /// sample from a peer that does not write it.
    #[serde(default)]
    pub version: Option<String>,
    /// The tag ready on this machine, like `v0.9.1`. `None` in a sample from a peer that does
    /// not write it.
    #[serde(default)]
    pub update_ready: Option<String>,
}

/// Any kind of record, tagged by `kind`, so one message can carry several kinds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AnyRecord {
    Workspace(Record<Workspace>),
    Terminal(Record<Terminal>),
    Session(Record<Session>),
    MachineSample(Record<MachineSample>),
}

impl AnyRecord {
    pub fn owner(&self) -> MachineId {
        match self {
            Self::Workspace(r) => r.owner,
            Self::Terminal(r) => r.owner,
            Self::Session(r) => r.owner,
            Self::MachineSample(r) => r.owner,
        }
    }

    pub fn version(&self) -> u64 {
        match self {
            Self::Workspace(r) => r.version,
            Self::Terminal(r) => r.version,
            Self::Session(r) => r.version,
            Self::MachineSample(r) => r.version,
        }
    }

    /// [`Record::supersedes`] for two records of the same kind. Never true across kinds.
    pub fn supersedes(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Workspace(a), Self::Workspace(b)) => a.supersedes(b),
            (Self::Terminal(a), Self::Terminal(b)) => a.supersedes(b),
            (Self::Session(a), Self::Session(b)) => a.supersedes(b),
            (Self::MachineSample(a), Self::MachineSample(b)) => a.supersedes(b),
            _ => false,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{assert_round_trip, parse_error};

    pub(crate) fn workspace() -> Record<Workspace> {
        Record {
            owner: MachineId::random(),
            version: 7,
            deleted: false,
            body: Workspace {
                id: WorkspaceId::random(),
                project: ProjectId::random(),
                name: "fix login".into(),
                branch: "fix-login".into(),
                base: "main".into(),
                setup: Some(TerminalId::random()),
            },
        }
    }

    pub(crate) fn terminal() -> Record<Terminal> {
        Record {
            owner: MachineId::random(),
            version: 3,
            deleted: false,
            body: Terminal {
                id: TerminalId::random(),
                workspace: WorkspaceId::random(),
                state: TerminalState::Exited,
                exit_code: Some(1),
            },
        }
    }

    pub(crate) fn session() -> Record<Session> {
        Record {
            owner: MachineId::random(),
            version: 12,
            deleted: true,
            body: Session {
                id: SessionId::random(),
                workspace: WorkspaceId::random(),
                terminal: TerminalId::random(),
                model: "claude-opus-5-5".into(),
                state: SessionState::WaitingForYou,
                name: Some("tidy the cart".into()),
            },
        }
    }

    pub(crate) fn machine_sample() -> Record<MachineSample> {
        Record {
            owner: MachineId::random(),
            version: u64::MAX,
            deleted: false,
            body: MachineSample {
                load_per_core: 1.25,
                free_memory: 8 * 1024 * 1024 * 1024,
                free_disk: 512 * 1024 * 1024 * 1024,
                sessions: 4,
                update_refused: Some("v0.2.0".into()),
                version: None,
                update_ready: None,
            },
        }
    }

    #[test]
    fn every_record_round_trips() {
        assert_round_trip(&workspace());
        assert_round_trip(&terminal());
        assert_round_trip(&session());
        assert_round_trip(&machine_sample());
    }

    #[test]
    fn every_kind_of_record_round_trips_as_any_record() {
        assert_round_trip(&AnyRecord::Workspace(workspace()));
        assert_round_trip(&AnyRecord::Terminal(terminal()));
        assert_round_trip(&AnyRecord::Session(session()));
        assert_round_trip(&AnyRecord::MachineSample(machine_sample()));
    }

    #[test]
    fn a_workspace_with_no_setup_and_a_sample_with_no_refused_tag_round_trip() {
        let mut workspace = workspace();
        workspace.body.setup = None;
        assert_round_trip(&workspace);
        let mut sample = machine_sample();
        sample.body.update_refused = None;
        assert_round_trip(&sample);
    }

    #[test]
    fn a_setup_terminal_and_a_refused_tag_are_written_under_their_names() {
        let workspace = workspace();
        let json = serde_json::to_value(&workspace).unwrap();
        assert_eq!(
            json["body"]["setup"],
            serde_json::json!(workspace.body.setup.unwrap())
        );
        let json = serde_json::to_value(machine_sample()).unwrap();
        assert_eq!(json["body"]["update_refused"], "v0.2.0");
    }

    #[test]
    fn a_session_with_no_name_reads_as_unnamed_and_one_with_a_name_writes_it_under_its_name() {
        let mut json = serde_json::to_value(session().body).unwrap();
        assert_eq!(json["name"], "tidy the cart");
        json.as_object_mut().unwrap().remove("name");
        let unnamed: Session = serde_json::from_value(json).unwrap();
        assert_eq!(unnamed.name, None);
        let mut record = session();
        record.body.name = None;
        assert_round_trip(&record);
    }

    #[test]
    fn a_sample_with_no_version_and_no_ready_tag_reads_with_neither() {
        let json = serde_json::json!({
            "load_per_core": 0.5,
            "free_memory": 1,
            "free_disk": 2,
            "sessions": 3,
        });
        let sample: MachineSample = serde_json::from_value(json).unwrap();
        assert_eq!(sample.version, None);
        assert_eq!(sample.update_ready, None);
    }

    #[test]
    fn a_sample_with_a_version_and_a_ready_tag_round_trips_under_their_names() {
        let mut record = machine_sample();
        record.body.version = Some("0.9.0".into());
        record.body.update_ready = Some("v0.9.1".into());
        assert_round_trip(&record);
        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(json["body"]["version"], "0.9.0");
        assert_eq!(json["body"]["update_ready"], "v0.9.1");
        assert_eq!(json["version"], u64::MAX);
        let back: Record<MachineSample> = serde_json::from_value(json).unwrap();
        assert_eq!(back.body.version.as_deref(), Some("0.9.0"));
        assert_eq!(back.body.update_ready.as_deref(), Some("v0.9.1"));
    }

    #[test]
    fn a_terminal_with_an_unknown_exit_code_round_trips() {
        let mut record = terminal();
        record.body.exit_code = None;
        assert_round_trip(&record);
    }

    fn every_kind() -> Vec<AnyRecord> {
        vec![
            AnyRecord::Workspace(workspace()),
            AnyRecord::Terminal(terminal()),
            AnyRecord::Session(session()),
            AnyRecord::MachineSample(machine_sample()),
        ]
    }

    #[test]
    fn an_unknown_field_is_ignored() {
        let record = workspace();
        let mut json = serde_json::to_value(&record).unwrap();
        json["added_later"] = serde_json::json!(true);
        json["body"]["also_added_later"] = serde_json::json!("x");
        let back: Record<Workspace> = serde_json::from_value(json).unwrap();
        assert_eq!(back, record);
    }

    #[test]
    fn an_unknown_field_in_any_kind_of_record_or_its_body_is_ignored() {
        for record in every_kind() {
            let mut json = serde_json::to_value(&record).unwrap();
            json["added_later"] = serde_json::json!(1);
            json["body"]["also_added_later"] = serde_json::json!({ "x": [1, 2] });
            let back: AnyRecord = serde_json::from_value(json).unwrap();
            assert_eq!(back, record);
        }
    }

    #[test]
    fn every_kind_of_record_reads_from_its_wire_form() {
        let owner = "67e55044-10b1-426f-9247-bb680e5fe0c8";
        let a = "11111111-1111-4111-8111-111111111111";
        let b = "22222222-2222-4222-8222-222222222222";
        let c = "33333333-3333-4333-8333-333333333333";
        let id = |s: &str| s.parse::<uuid::Uuid>().unwrap();
        let wire = |kind: &str, body: String| {
            format!(
                r#"{{"kind":"{kind}","owner":"{owner}","version":5,"deleted":false,"body":{body}}}"#
            )
        };
        let cases = [
            (
                wire(
                    "workspace",
                    format!(
                        r#"{{"id":"{a}","project":"{b}","name":"n","branch":"b","base":"main"}}"#
                    ),
                ),
                AnyRecord::Workspace(Record {
                    owner: MachineId(id(owner)),
                    version: 5,
                    deleted: false,
                    body: Workspace {
                        id: WorkspaceId(id(a)),
                        project: ProjectId(id(b)),
                        name: "n".into(),
                        branch: "b".into(),
                        base: "main".into(),
                        setup: None,
                    },
                }),
            ),
            (
                wire(
                    "terminal",
                    format!(r#"{{"id":"{a}","workspace":"{b}","state":"exited","exit_code":-1}}"#),
                ),
                AnyRecord::Terminal(Record {
                    owner: MachineId(id(owner)),
                    version: 5,
                    deleted: false,
                    body: Terminal {
                        id: TerminalId(id(a)),
                        workspace: WorkspaceId(id(b)),
                        state: TerminalState::Exited,
                        exit_code: Some(-1),
                    },
                }),
            ),
            (
                wire(
                    "session",
                    format!(
                        r#"{{"id":"{a}","workspace":"{b}","terminal":"{c}","model":"m","state":"waiting_for_you"}}"#
                    ),
                ),
                AnyRecord::Session(Record {
                    owner: MachineId(id(owner)),
                    version: 5,
                    deleted: false,
                    body: Session {
                        id: SessionId(id(a)),
                        workspace: WorkspaceId(id(b)),
                        terminal: TerminalId(id(c)),
                        model: "m".into(),
                        state: SessionState::WaitingForYou,
                        name: None,
                    },
                }),
            ),
            (
                wire(
                    "machine_sample",
                    r#"{"load_per_core":0.5,"free_memory":1,"free_disk":2,"sessions":3}"#
                        .to_string(),
                ),
                AnyRecord::MachineSample(Record {
                    owner: MachineId(id(owner)),
                    version: 5,
                    deleted: false,
                    body: MachineSample {
                        load_per_core: 0.5,
                        free_memory: 1,
                        free_disk: 2,
                        sessions: 3,
                        update_refused: None,
                        version: None,
                        update_ready: None,
                    },
                }),
            ),
        ];
        for (json, expected) in cases {
            let parsed: AnyRecord = serde_json::from_str(&json).expect(&json);
            assert_eq!(parsed, expected, "{json}");
        }
    }

    #[test]
    fn any_record_gives_its_owner_and_version() {
        for record in every_kind() {
            let json = serde_json::to_value(&record).unwrap();
            assert_eq!(record.owner().to_string(), json["owner"].as_str().unwrap());
            assert_eq!(record.version(), json["version"].as_u64().unwrap());
        }
    }

    fn with_version(record: &AnyRecord, owner: MachineId, version: u64) -> AnyRecord {
        let mut json = serde_json::to_value(record).unwrap();
        json["owner"] = serde_json::json!(owner);
        json["version"] = serde_json::json!(version);
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn any_record_supersedes_by_the_same_rule_as_record_for_every_kind() {
        for older in every_kind() {
            let owner = older.owner();
            let older = with_version(&older, owner, 7);
            let newer = with_version(&older, owner, 8);
            let other_owner = with_version(&older, MachineId::random(), 9);
            assert!(newer.supersedes(&older), "{newer:?}");
            assert!(!older.supersedes(&newer), "{older:?}");
            assert!(!older.supersedes(&older.clone()), "{older:?}");
            assert!(!other_owner.supersedes(&older), "{other_owner:?}");
            assert!(!older.supersedes(&other_owner), "{older:?}");
        }
    }

    #[test]
    fn a_sample_with_a_non_finite_load_cannot_be_written() {
        for load in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut record = machine_sample();
            record.body.load_per_core = load;
            assert!(serde_json::to_string(&record).is_err(), "{load}");
            assert!(
                serde_json::to_string(&AnyRecord::MachineSample(record)).is_err(),
                "{load}"
            );
        }
    }

    #[test]
    fn a_record_of_another_kind_never_supersedes() {
        let workspace = workspace();
        let mut session = session();
        session.owner = workspace.owner;
        session.version = workspace.version + 1;
        let workspace = AnyRecord::Workspace(workspace);
        let session = AnyRecord::Session(session);
        assert!(!session.supersedes(&workspace));
        assert!(!workspace.supersedes(&session));
    }

    #[test]
    fn an_unknown_kind_of_record_is_an_error_that_names_it() {
        let mut json = serde_json::to_value(AnyRecord::Workspace(workspace())).unwrap();
        json["kind"] = serde_json::json!("printer");
        assert!(parse_error::<AnyRecord>(&json.to_string()).contains("printer"));
    }

    #[test]
    fn an_unknown_state_inside_a_record_is_an_error_that_names_it() {
        let mut json = serde_json::to_value(session()).unwrap();
        json["body"]["state"] = serde_json::json!("dreaming");
        assert!(parse_error::<Record<Session>>(&json.to_string()).contains("dreaming"));
    }

    #[test]
    fn a_higher_version_from_the_same_owner_supersedes() {
        let older = workspace();
        let newer = Record {
            version: older.version + 1,
            ..older.clone()
        };
        assert!(newer.supersedes(&older));
        assert!(!older.supersedes(&newer));
    }

    #[test]
    fn an_equal_version_does_not_supersede() {
        let record = workspace();
        assert!(!record.supersedes(&record.clone()));
    }

    #[test]
    fn a_higher_version_from_another_owner_does_not_supersede() {
        let mine = workspace();
        let theirs = Record {
            owner: MachineId::random(),
            version: mine.version + 100,
            ..mine.clone()
        };
        assert!(!theirs.supersedes(&mine));
        assert!(!mine.supersedes(&theirs));
    }

    #[test]
    fn a_deleted_record_with_a_higher_version_supersedes_a_live_one() {
        let live = workspace();
        let deleted = Record {
            version: live.version + 1,
            deleted: true,
            ..live.clone()
        };
        assert!(deleted.supersedes(&live));
        assert!(!live.supersedes(&deleted));
    }
}

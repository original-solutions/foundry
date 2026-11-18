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
}

/// A PTY in a workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Terminal {
    pub id: TerminalId,
    pub workspace: WorkspaceId,
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
    pub state: SessionState,
}

/// The owner machine's load, memory, disk and session count.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MachineSample {
    /// The 1-minute load divided by logical cores.
    pub load_per_core: f64,
    /// Bytes.
    pub free_memory: u64,
    /// Bytes free on the volume holding the data directory.
    pub free_disk: u64,
    pub sessions: u32,
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ids::{MachineId, ProjectId, SessionId, TerminalId, WorkspaceId};
    use crate::state::{SessionState, TerminalState};
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
    fn a_terminal_with_an_unknown_exit_code_round_trips() {
        let mut record = terminal();
        record.body.exit_code = None;
        assert_round_trip(&record);
    }

    #[test]
    fn an_unknown_field_is_ignored() {
        let record = workspace();
        let mut json = serde_json::to_value(&record).unwrap();
        json["added_later"] = serde_json::json!(true);
        json["body"]["also_added_later"] = serde_json::json!("x");
        let back: Record<Workspace> = serde_json::from_value(json).unwrap();
        assert_eq!(back, record);

        let any = AnyRecord::Session(session());
        let mut json = serde_json::to_value(&any).unwrap();
        json["added_later"] = serde_json::json!(1);
        let back: AnyRecord = serde_json::from_value(json).unwrap();
        assert_eq!(back, any);
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

//! UUID newtypes for machines, projects, workspaces, terminals and sessions.

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! uuid_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            /// A new random id.
            pub fn random() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

uuid_id!(
    /// A machine in the fleet.
    MachineId
);
uuid_id!(
    /// A project: a repo URL and a name.
    ProjectId
);
uuid_id!(
    /// A workspace: a worktree and branch on one machine.
    WorkspaceId
);
uuid_id!(
    /// A terminal: one PTY owned by a holder.
    TerminalId
);
uuid_id!(
    /// An agent session: a terminal running Claude Code.
    SessionId
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{assert_round_trip, parse_error};

    const UUID: &str = "67e55044-10b1-426f-9247-bb680e5fe0c8";

    #[test]
    fn every_id_round_trips() {
        assert_round_trip(&MachineId::random());
        assert_round_trip(&ProjectId::random());
        assert_round_trip(&WorkspaceId::random());
        assert_round_trip(&TerminalId::random());
        assert_round_trip(&SessionId::random());
    }

    #[test]
    fn an_id_is_a_plain_uuid_string() {
        let id: MachineId = serde_json::from_str(&format!("\"{UUID}\"")).unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), format!("\"{UUID}\""));
        assert_eq!(id.to_string(), UUID);
    }

    #[test]
    fn random_ids_differ() {
        assert_ne!(SessionId::random(), SessionId::random());
    }

    #[test]
    fn a_string_that_is_not_a_uuid_is_refused() {
        parse_error::<WorkspaceId>("\"not-a-uuid\"");
    }
}

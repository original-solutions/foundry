//! Session and terminal states.

use serde::{Deserialize, Serialize};

/// The states an agent session can show, from the spec's state table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// Launched and has not reported yet.
    Starting,
    /// The agent is running a turn.
    Working,
    /// The agent needs a permission answer or other input.
    WaitingForYou,
    /// The turn ended and the agent waits for the next prompt.
    Idle,
    /// The session ended normally.
    Finished,
    /// The process died or exited non-zero.
    Failed,
    /// Working, with no output and no hook event for 10 minutes.
    Stuck,
    /// The owning machine is offline.
    Unreachable,
}

/// The states a shell terminal can show.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalState {
    /// The child process is running.
    Running,
    /// The child process has exited.
    Exited,
    /// The owning machine is offline.
    Unreachable,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{assert_round_trip, parse_error};

    #[test]
    fn every_session_state_round_trips_under_its_spec_name() {
        let states = [
            (SessionState::Starting, "starting"),
            (SessionState::Working, "working"),
            (SessionState::WaitingForYou, "waiting_for_you"),
            (SessionState::Idle, "idle"),
            (SessionState::Finished, "finished"),
            (SessionState::Failed, "failed"),
            (SessionState::Stuck, "stuck"),
            (SessionState::Unreachable, "unreachable"),
        ];
        for (state, name) in states {
            assert_round_trip(&state);
            assert_eq!(
                serde_json::to_string(&state).unwrap(),
                format!("\"{name}\"")
            );
        }
    }

    #[test]
    fn every_terminal_state_round_trips_under_its_spec_name() {
        let states = [
            (TerminalState::Running, "running"),
            (TerminalState::Exited, "exited"),
            (TerminalState::Unreachable, "unreachable"),
        ];
        for (state, name) in states {
            assert_round_trip(&state);
            assert_eq!(
                serde_json::to_string(&state).unwrap(),
                format!("\"{name}\"")
            );
        }
    }

    #[test]
    fn an_unknown_state_is_an_error_that_names_it() {
        assert!(parse_error::<SessionState>("\"paused\"").contains("paused"));
        assert!(parse_error::<TerminalState>("\"sleeping\"").contains("sleeping"));
    }
}

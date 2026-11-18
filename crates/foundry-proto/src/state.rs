//! Session and terminal states.

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

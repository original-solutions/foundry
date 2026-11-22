//! The rules that turn what a session's agent does into the session's state. Pure: no clock,
//! no I/O, so every row of the spec's state table is a unit test.

use foundry_proto::state::SessionState;

/// A Claude Code hook event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hook {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    Notification,
    Stop,
    SessionEnd,
    /// Any event the session state does not depend on.
    Other,
}

impl Hook {
    /// The hook a `hook_event_name` names. An unknown name is [`Hook::Other`].
    pub fn from_name(name: &str) -> Self {
        match name {
            "SessionStart" => Self::SessionStart,
            "UserPromptSubmit" => Self::UserPromptSubmit,
            "PreToolUse" => Self::PreToolUse,
            "PostToolUse" => Self::PostToolUse,
            "Notification" => Self::Notification,
            "Stop" => Self::Stop,
            "SessionEnd" => Self::SessionEnd,
            _ => Self::Other,
        }
    }

    /// The hook a posted payload is, or `None` when it names none. `SessionEnd` with reason
    /// `clear` is [`Hook::Other`]: Claude Code sends it, then `SessionStart` with source
    /// `clear`, and keeps running.
    pub fn from_payload(payload: &serde_json::Value) -> Option<Self> {
        let hook = Self::from_name(payload.get("hook_event_name")?.as_str()?);
        let cleared = payload.get("reason").and_then(|r| r.as_str()) == Some("clear");
        Some(if hook == Self::SessionEnd && cleared {
            Self::Other
        } else {
            hook
        })
    }
}

/// Something that happened to a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The agent posted a hook.
    Hook(Hook),
    /// The terminal wrote output.
    Output,
    /// Nothing was heard from a working session for the stuck time.
    Stalled,
    /// The child exited with this code, or none known.
    Exited(Option<i32>),
}

/// The state after `event`. The owner never produces `Unreachable`: only a receiver shows it,
/// and a state that is `Unreachable` on the way in is left alone.
pub fn next_state(state: SessionState, event: Event) -> SessionState {
    use SessionState::{
        Failed, Finished, Idle, Starting, Stuck, Unreachable, WaitingForYou, Working,
    };
    match state {
        Unreachable | Failed => return state,
        // Finished is final, except that the child can still turn out to have exited badly.
        Finished => {
            return match event {
                Event::Exited(code) if code != Some(0) => Failed,
                _ => Finished,
            };
        }
        _ => {}
    }
    match event {
        Event::Exited(Some(0)) => Finished,
        Event::Exited(_) => Failed,
        Event::Hook(hook) => match hook {
            Hook::SessionEnd => Finished,
            // It also fires on /clear, resume and compact, sometimes mid-turn.
            Hook::SessionStart if state == Starting => Idle,
            // Any event clears stuck, as output does.
            Hook::SessionStart | Hook::Other if state == Stuck => Working,
            Hook::UserPromptSubmit | Hook::PreToolUse | Hook::PostToolUse => Working,
            // Claude Code also notifies after 60 seconds idle: an idle session is not waiting
            // on a permission answer.
            Hook::Notification if matches!(state, Starting | Working | Stuck) => WaitingForYou,
            Hook::Stop => Idle,
            Hook::SessionStart | Hook::Notification | Hook::Other => state,
        },
        Event::Output if state == Stuck => Working,
        Event::Stalled if state == Working => Stuck,
        Event::Output | Event::Stalled => state,
    }
}

#[cfg(test)]
mod tests {
    use SessionState::*;
    use foundry_proto::state::SessionState;

    use super::*;

    const HOOKS: [Hook; 8] = [
        Hook::SessionStart,
        Hook::UserPromptSubmit,
        Hook::PreToolUse,
        Hook::PostToolUse,
        Hook::Notification,
        Hook::Stop,
        Hook::SessionEnd,
        Hook::Other,
    ];

    const STATES: [SessionState; 8] = [
        Starting,
        Working,
        WaitingForYou,
        Idle,
        Finished,
        Failed,
        Stuck,
        Unreachable,
    ];

    fn events() -> Vec<Event> {
        let mut events: Vec<Event> = HOOKS.iter().copied().map(Event::Hook).collect();
        events.extend([
            Event::Output,
            Event::Stalled,
            Event::Exited(Some(0)),
            Event::Exited(Some(3)),
            Event::Exited(None),
        ]);
        events
    }

    #[test]
    fn hook_names_map_to_their_hooks_and_unknown_names_to_other() {
        assert_eq!(Hook::from_name("SessionStart"), Hook::SessionStart);
        assert_eq!(Hook::from_name("UserPromptSubmit"), Hook::UserPromptSubmit);
        assert_eq!(Hook::from_name("PreToolUse"), Hook::PreToolUse);
        assert_eq!(Hook::from_name("PostToolUse"), Hook::PostToolUse);
        assert_eq!(Hook::from_name("Notification"), Hook::Notification);
        assert_eq!(Hook::from_name("Stop"), Hook::Stop);
        assert_eq!(Hook::from_name("SessionEnd"), Hook::SessionEnd);
        assert_eq!(Hook::from_name("PreCompact"), Hook::Other);
        assert_eq!(Hook::from_name(""), Hook::Other);
    }

    #[test]
    fn session_start_moves_starting_to_idle_and_changes_nothing_else() {
        assert_eq!(next_state(Starting, Event::Hook(Hook::SessionStart)), Idle);
        for state in [Working, WaitingForYou, Idle] {
            assert_eq!(next_state(state, Event::Hook(Hook::SessionStart)), state);
        }
    }

    #[test]
    fn any_hook_clears_stuck() {
        // The spec: stuck clears when output or an event arrives.
        for hook in [Hook::SessionStart, Hook::Other] {
            assert_eq!(next_state(Stuck, Event::Hook(hook)), Working, "{hook:?}");
        }
    }

    #[test]
    fn a_session_end_that_is_a_clear_is_not_an_end() {
        let payload = |reason: Option<&str>| {
            let mut v = serde_json::json!({"hook_event_name": "SessionEnd"});
            if let Some(reason) = reason {
                v["reason"] = reason.into();
            }
            Hook::from_payload(&v)
        };
        assert_eq!(payload(Some("clear")), Some(Hook::Other));
        assert_eq!(payload(Some("resume")), Some(Hook::Other));
        for reason in [
            None,
            Some("logout"),
            Some("other"),
            Some("prompt_input_exit"),
        ] {
            assert_eq!(payload(reason), Some(Hook::SessionEnd), "{reason:?}");
        }
        let named = |name: &str| Hook::from_payload(&serde_json::json!({"hook_event_name": name}));
        assert_eq!(named("Stop"), Some(Hook::Stop));
        assert_eq!(named("Whatever"), Some(Hook::Other));
        assert_eq!(Hook::from_payload(&serde_json::json!({"a": 1})), None);
        assert_eq!(
            Hook::from_payload(&serde_json::json!({"hook_event_name": 7})),
            None
        );
    }

    #[test]
    fn prompts_and_tool_hooks_mean_working_from_every_live_state() {
        for hook in [Hook::UserPromptSubmit, Hook::PreToolUse, Hook::PostToolUse] {
            for state in [Starting, Working, WaitingForYou, Idle, Stuck] {
                assert_eq!(next_state(state, Event::Hook(hook)), Working, "{hook:?}");
            }
        }
    }

    #[test]
    fn a_notification_means_waiting_only_while_the_agent_is_busy() {
        for state in [Starting, Working, Stuck] {
            assert_eq!(
                next_state(state, Event::Hook(Hook::Notification)),
                WaitingForYou
            );
        }
        // Claude Code also notifies after 60 seconds idle.
        for state in [Idle, WaitingForYou] {
            assert_eq!(next_state(state, Event::Hook(Hook::Notification)), state);
        }
    }

    #[test]
    fn stop_means_idle() {
        for state in [Starting, Working, WaitingForYou, Idle, Stuck] {
            assert_eq!(next_state(state, Event::Hook(Hook::Stop)), Idle);
        }
    }

    #[test]
    fn session_end_means_finished() {
        for state in [Starting, Working, WaitingForYou, Idle, Stuck] {
            assert_eq!(next_state(state, Event::Hook(Hook::SessionEnd)), Finished);
        }
    }

    #[test]
    fn an_other_hook_changes_nothing_but_stuck() {
        for state in STATES.into_iter().filter(|s| *s != Stuck) {
            assert_eq!(next_state(state, Event::Hook(Hook::Other)), state);
        }
    }

    #[test]
    fn output_clears_stuck_and_changes_nothing_else() {
        assert_eq!(next_state(Stuck, Event::Output), Working);
        for state in [Starting, Working, WaitingForYou, Idle, Finished, Failed] {
            assert_eq!(next_state(state, Event::Output), state);
        }
    }

    #[test]
    fn stalled_turns_working_into_stuck_and_changes_nothing_else() {
        assert_eq!(next_state(Working, Event::Stalled), Stuck);
        for state in [Starting, WaitingForYou, Idle, Stuck, Finished, Failed] {
            assert_eq!(next_state(state, Event::Stalled), state);
        }
    }

    #[test]
    fn an_exit_with_code_zero_finishes_and_any_other_exit_fails() {
        for state in [Starting, Working, WaitingForYou, Idle, Stuck] {
            assert_eq!(next_state(state, Event::Exited(Some(0))), Finished);
            assert_eq!(next_state(state, Event::Exited(Some(3))), Failed);
            assert_eq!(next_state(state, Event::Exited(None)), Failed);
        }
    }

    #[test]
    fn failed_is_final() {
        for event in events() {
            assert_eq!(next_state(Failed, event), Failed, "{event:?}");
        }
    }

    #[test]
    fn finished_is_final_except_that_a_bad_exit_after_it_fails() {
        for event in events() {
            let want = match event {
                Event::Exited(Some(3) | None) => Failed,
                _ => Finished,
            };
            assert_eq!(next_state(Finished, event), want, "{event:?}");
        }
    }

    #[test]
    fn unreachable_in_is_unchanged() {
        for event in events() {
            assert_eq!(next_state(Unreachable, event), Unreachable, "{event:?}");
        }
    }

    #[test]
    fn the_owner_never_produces_unreachable() {
        for state in STATES.into_iter().filter(|s| *s != Unreachable) {
            for event in events() {
                assert_ne!(next_state(state, event), Unreachable, "{state:?} {event:?}");
            }
        }
    }
}

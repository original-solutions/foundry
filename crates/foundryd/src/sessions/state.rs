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
        for state in [Working, WaitingForYou, Idle, Stuck] {
            assert_eq!(next_state(state, Event::Hook(Hook::SessionStart)), state);
        }
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
    fn an_other_hook_changes_nothing() {
        for state in STATES {
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
        for state in STATES {
            for event in events() {
                assert_ne!(next_state(state, event), Unreachable, "{state:?} {event:?}");
            }
        }
    }
}

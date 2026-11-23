//! The factory states of an issue and the moves between them. Pure.

#[cfg(test)]
mod tests {
    use super::*;

    /// The label table of softfac's docs/lifecycle.md, pinned: the first two columns of each row.
    const LABEL_TABLE_ROWS: [&str; 7] = [
        "| `factory:proposed` | A bot filed it. The factory ignores it until a person sets `factory:ready`. |",
        "| `factory:ready` | Approved for the factory. |",
        "| `factory:triage` | Claimed. The session is being launched. |",
        "| `factory:needs-info` | The factory asked questions on the issue. Answer them, then set `factory:ready`. |",
        "| `factory:building` | A session is building it. |",
        "| `factory:pr-open` | The PR is open and has passed the gate. |",
        "| `factory:stuck` | The factory gave up and said why in a comment. |",
    ];

    fn ok(state: State, event: Event) -> (State, Vec<Action>) {
        match transition(state, &event) {
            Ok(t) => t,
            Err(e) => panic!("{e}"),
        }
    }

    fn invalid(state: State, event: &Event) -> Option<InvalidTransition> {
        transition(state, event).err()
    }

    fn stuck_actions(reason: &str) -> Vec<Action> {
        vec![
            Action::PostStuckComment {
                reason: reason.into(),
            },
            Action::SetLabel {
                state: State::Stuck,
            },
            Action::Unassign,
            Action::UpdateStatusComment,
        ]
    }

    fn questions_actions(questions: &str) -> Vec<Action> {
        vec![
            Action::PostQuestions {
                questions: questions.into(),
            },
            Action::SetLabel {
                state: State::NeedsInfo,
            },
            Action::Unassign,
            Action::UpdateStatusComment,
        ]
    }

    // lifecycle states

    #[test]
    fn every_state_has_a_distinct_six_digit_colour() {
        let mut seen: Vec<&str> = Vec::new();
        for state in State::ALL {
            let color = state.color();
            assert!(
                color.len() == 6
                    && color
                        .chars()
                        .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
                "{color}"
            );
            assert!(!seen.contains(&color), "{color} is used twice");
            seen.push(color);
        }
    }

    #[test]
    fn the_descriptions_are_the_meanings_in_the_lifecycle_label_table() {
        assert_eq!(LABEL_TABLE_ROWS.len(), State::ALL.len());
        for state in State::ALL {
            let row = format!("| `{}` | {} |", state.label(), state.description());
            assert!(
                LABEL_TABLE_ROWS.contains(&row.as_str()),
                "the label table has no row {row}"
            );
            // GitHub cuts a label description at 100 characters.
            assert!(state.description().chars().count() <= 100);
        }
    }

    #[test]
    fn labels_round_trip() {
        let expected = [
            ("proposed", "factory:proposed"),
            ("ready", "factory:ready"),
            ("triage", "factory:triage"),
            ("needs-info", "factory:needs-info"),
            ("building", "factory:building"),
            ("pr-open", "factory:pr-open"),
            ("stuck", "factory:stuck"),
        ];
        assert_eq!(expected.len(), State::ALL.len());
        assert_eq!(
            State::ALL.map(State::name).to_vec(),
            expected.map(|(name, _)| name).to_vec()
        );
        for (state, (name, label)) in State::ALL.into_iter().zip(expected) {
            assert_eq!(state.name(), name);
            assert_eq!(state.to_string(), name);
            assert_eq!(state.label(), label);
            assert_eq!(State::from_label(label), Some(state));
        }
    }

    #[test]
    fn unknown_labels_are_rejected() {
        assert_eq!(State::from_label("factory:nope"), None);
        assert_eq!(State::from_label("ready"), None);
        assert_eq!(State::from_label(""), None);
        assert_eq!(State::from_label("Factory:ready"), None);
    }

    // lifecycle transitions

    #[test]
    fn proposed_approved() {
        let (next, actions) = ok(State::Proposed, Event::Approved);
        assert_eq!(next, State::Ready);
        assert_eq!(actions, vec![]);
    }

    #[test]
    fn ready_unapproved() {
        let (next, actions) = ok(
            State::Ready,
            Event::Unapproved {
                reason: "edited".into(),
            },
        );
        assert_eq!(next, State::Proposed);
        assert_eq!(
            actions,
            vec![
                Action::PostUnapproved {
                    reason: "edited".into()
                },
                Action::SetLabel {
                    state: State::Proposed
                },
            ]
        );
    }

    #[test]
    fn ready_claimed() {
        let (next, actions) = ok(State::Ready, Event::Claimed);
        assert_eq!(next, State::Triage);
        assert_eq!(
            actions,
            vec![
                Action::AssignToMachine,
                Action::SetLabel {
                    state: State::Triage
                },
            ]
        );
    }

    #[test]
    fn triage_session_starting() {
        let (next, actions) = ok(State::Triage, Event::SessionStarting);
        assert_eq!(next, State::Building);
        assert_eq!(
            actions,
            vec![
                Action::SetLabel {
                    state: State::Building
                },
                Action::LaunchSession,
                Action::UpdateStatusComment,
            ]
        );
    }

    #[test]
    fn triage_questions_raised() {
        let (next, actions) = ok(
            State::Triage,
            Event::QuestionsRaised {
                questions: "why?".into(),
            },
        );
        assert_eq!(next, State::NeedsInfo);
        assert_eq!(actions, questions_actions("why?"));
    }

    #[test]
    fn triage_failed() {
        let (next, actions) = ok(
            State::Triage,
            Event::Failed {
                reason: "boom".into(),
            },
        );
        assert_eq!(next, State::Stuck);
        assert_eq!(actions, stuck_actions("boom"));
    }

    #[test]
    fn building_instructor_commented() {
        let (next, actions) = ok(
            State::Building,
            Event::InstructorCommented {
                comment_id: "42".into(),
            },
        );
        assert_eq!(next, State::Building);
        assert_eq!(
            actions,
            vec![Action::ForwardComment {
                comment_id: "42".into()
            }]
        );
    }

    #[test]
    fn building_body_edited() {
        let (next, actions) = ok(State::Building, Event::BodyEdited);
        assert_eq!(next, State::Building);
        assert_eq!(actions, vec![Action::ForwardBodyEdit]);
    }

    #[test]
    fn building_gate_failed() {
        let (next, actions) = ok(
            State::Building,
            Event::GateFailed {
                output: "red".into(),
            },
        );
        assert_eq!(next, State::Building);
        assert_eq!(
            actions,
            vec![Action::SendGateFailure {
                output: "red".into()
            }]
        );
    }

    #[test]
    fn building_pr_ready() {
        let (next, actions) = ok(State::Building, Event::PrReady { pr: 9 });
        assert_eq!(next, State::PrOpen);
        assert_eq!(
            actions,
            vec![
                Action::MarkPrReady { pr: 9 },
                Action::PostGateResults { pr: 9 },
                Action::SetLabel {
                    state: State::PrOpen
                },
                Action::UpdateStatusComment,
            ]
        );
    }

    #[test]
    fn pr_open_failed() {
        let (next, actions) = ok(State::PrOpen, Event::Failed { reason: "r".into() });
        assert_eq!(next, State::Stuck);
        assert_eq!(actions, stuck_actions("r"));
    }

    #[test]
    fn pr_open_regate_failed_puts_the_pr_back_to_draft_and_the_issue_back_to_building() {
        let (next, actions) = ok(
            State::PrOpen,
            Event::RegateFailed {
                pr: 12,
                output: "boom".into(),
            },
        );
        assert_eq!(next, State::Building);
        assert_eq!(
            actions,
            vec![
                Action::MarkPrDraft { pr: 12 },
                Action::AssignToMachine,
                Action::SetLabel {
                    state: State::Building
                },
                Action::SendGateFailure {
                    output: "boom".into()
                },
                Action::UpdateStatusComment,
            ]
        );
    }

    #[test]
    fn pr_open_instructor_feedback_puts_the_pr_back_to_draft_and_the_issue_straight_to_building() {
        let (next, actions) = ok(State::PrOpen, Event::InstructorFeedback { pr: 12 });
        assert_eq!(next, State::Building);
        assert_eq!(
            actions,
            vec![
                Action::MarkPrDraft { pr: 12 },
                Action::AssignToMachine,
                Action::SetLabel {
                    state: State::Building
                },
                Action::UpdateStatusComment,
            ]
        );
    }

    #[test]
    fn building_questions_raised() {
        let (next, actions) = ok(
            State::Building,
            Event::QuestionsRaised {
                questions: "which?".into(),
            },
        );
        assert_eq!(next, State::NeedsInfo);
        assert_eq!(actions, questions_actions("which?"));
    }

    #[test]
    fn building_failed() {
        let (next, actions) = ok(
            State::Building,
            Event::Failed {
                reason: "timeout".into(),
            },
        );
        assert_eq!(next, State::Stuck);
        assert_eq!(actions, stuck_actions("timeout"));
    }

    #[test]
    fn needs_info_approved() {
        let (next, actions) = ok(State::NeedsInfo, Event::Approved);
        assert_eq!(next, State::Ready);
        assert_eq!(actions, vec![]);
    }

    #[test]
    fn stuck_approved() {
        let (next, actions) = ok(State::Stuck, Event::Approved);
        assert_eq!(next, State::Ready);
        assert_eq!(actions, vec![]);
    }

    // lifecycle transition table

    /// Every event kind with a sample value and its hand-written expected name.
    fn every_event() -> Vec<(Event, &'static str)> {
        vec![
            (Event::Approved, "approved"),
            (Event::Claimed, "claimed"),
            (Event::SessionStarting, "session-starting"),
            (
                Event::QuestionsRaised {
                    questions: "q".into(),
                },
                "questions-raised",
            ),
            (Event::Failed { reason: "r".into() }, "failed"),
            (
                Event::InstructorCommented {
                    comment_id: "1".into(),
                },
                "instructor-commented",
            ),
            (Event::BodyEdited, "body-edited"),
            (Event::GateFailed { output: "o".into() }, "gate-failed"),
            (Event::PrReady { pr: 2 }, "pr-ready"),
            (Event::Unapproved { reason: "r".into() }, "unapproved"),
            (
                Event::RegateFailed {
                    pr: 3,
                    output: "o".into(),
                },
                "regate-failed",
            ),
            (Event::InstructorFeedback { pr: 4 }, "instructor-feedback"),
        ]
    }

    /// The place of each event kind in [`every_event`]. A match with no wildcard, so adding an
    /// `Event` kind fails to compile until it is listed.
    fn place(event: &Event) -> usize {
        match event {
            Event::Approved => 0,
            Event::Claimed => 1,
            Event::SessionStarting => 2,
            Event::QuestionsRaised { .. } => 3,
            Event::Failed { .. } => 4,
            Event::InstructorCommented { .. } => 5,
            Event::BodyEdited => 6,
            Event::GateFailed { .. } => 7,
            Event::PrReady { .. } => 8,
            Event::Unapproved { .. } => 9,
            Event::RegateFailed { .. } => 10,
            Event::InstructorFeedback { .. } => 11,
        }
    }

    const VALID: [(State, &str); 17] = [
        (State::Proposed, "approved"),
        (State::Ready, "claimed"),
        (State::Ready, "unapproved"),
        (State::Triage, "session-starting"),
        (State::Triage, "questions-raised"),
        (State::Triage, "failed"),
        (State::Building, "instructor-commented"),
        (State::Building, "body-edited"),
        (State::Building, "gate-failed"),
        (State::Building, "pr-ready"),
        (State::Building, "questions-raised"),
        (State::Building, "failed"),
        (State::PrOpen, "failed"),
        (State::PrOpen, "regate-failed"),
        (State::PrOpen, "instructor-feedback"),
        (State::NeedsInfo, "approved"),
        (State::Stuck, "approved"),
    ];

    #[test]
    fn every_event_is_listed() {
        let places: Vec<usize> = every_event().iter().map(|(e, _)| place(e)).collect();
        assert_eq!(places, (0..12).collect::<Vec<_>>());
    }

    #[test]
    fn event_names_are_pinned() {
        for (event, name) in every_event() {
            assert_eq!(event.name(), name);
        }
    }

    #[test]
    fn exactly_the_table_pairs_succeed() {
        let mut valid_seen = 0;
        for state in State::ALL {
            for (event, name) in every_event() {
                let error = invalid(state, &event);
                if VALID.contains(&(state, name)) {
                    assert!(error.is_none(), "{state} / {name} should be valid");
                    valid_seen += 1;
                } else {
                    let error =
                        error.unwrap_or_else(|| panic!("{state} / {name} should be invalid"));
                    assert_eq!(error.state, state);
                    assert_eq!(error.event, name);
                }
            }
        }
        assert_eq!(valid_seen, 17);
        assert_eq!(VALID.len(), 17);
    }

    #[test]
    fn pr_open_accepts_only_failed_regate_failed_and_instructor_feedback() {
        for (event, name) in every_event() {
            if matches!(name, "failed" | "regate-failed" | "instructor-feedback") {
                continue;
            }
            let error = invalid(State::PrOpen, &event).expect("invalid");
            assert_eq!(error.state, State::PrOpen);
            assert_eq!(error.event, name);
        }
    }

    #[test]
    fn an_invalid_pair_is_a_typed_failure_not_a_throw() {
        let result = transition(State::Building, &Event::Claimed);
        assert_eq!(
            result,
            Err(InvalidTransition {
                state: State::Building,
                event: "claimed"
            })
        );
    }

    #[test]
    fn invalid_transition_display() {
        assert_eq!(
            invalid(State::Building, &Event::Claimed)
                .expect("invalid")
                .to_string(),
            "event `claimed` is not valid in state `building`"
        );
        assert_eq!(
            invalid(State::PrOpen, &Event::PrReady { pr: 1 })
                .expect("invalid")
                .to_string(),
            "event `pr-ready` is not valid in state `pr-open`"
        );
    }

    #[test]
    fn set_label_always_matches_next_state() {
        for state in State::ALL {
            for (event, _) in every_event() {
                let Ok((next, actions)) = transition(state, &event) else {
                    continue;
                };
                let labels: Vec<State> = actions
                    .iter()
                    .filter_map(|a| match a {
                        Action::SetLabel { state } => Some(*state),
                        _ => None,
                    })
                    .collect();
                if labels.is_empty() {
                    // Self-loops keep their label; a person already moved proposed/needs-info/stuck to ready.
                    assert!(
                        next == state
                            || matches!(state, State::Proposed | State::NeedsInfo | State::Stuck),
                        "{state} -> {next} must set a label"
                    );
                } else {
                    assert_eq!(labels, vec![next]);
                }
            }
        }
    }

    #[test]
    fn assignment_follows_the_state_entered() {
        for state in State::ALL {
            for (event, _) in every_event() {
                let Ok((next, actions)) = transition(state, &event) else {
                    continue;
                };
                let unassigns = actions.iter().filter(|a| **a == Action::Unassign).count();
                let assigns = actions
                    .iter()
                    .filter(|a| **a == Action::AssignToMachine)
                    .count();
                // Only entering a state counts: a comment forwarded to the session is a self-loop.
                let entered = next != state;
                // Taking an issue (from `ready`) assigns it; giving it up releases it.
                let want = if matches!(next, State::NeedsInfo | State::Stuck) && entered {
                    (1, 0)
                } else if state == State::Ready && next == State::Triage {
                    (0, 1)
                } else if state == State::PrOpen && next == State::Building {
                    // A ready pull request whose gate failed again: the issue is ours again.
                    (0, 1)
                } else {
                    (0, 0)
                };
                assert_eq!((unassigns, assigns), want, "{state} -> {next}");
            }
        }
    }
}

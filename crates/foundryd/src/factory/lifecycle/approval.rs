//! Whether an issue is approved for the factory. Pure.
//!
//! The approval rule's verdicts and the words the factory says when it refuses. The rule itself
//! reads GitHub's history and lives with the tracker; this file holds what it concludes and
//! what is then written on the issue. Ported from the runner's `src/domain/approval.ts`.

/// Why an issue is not approved. A fixed set, so nothing from the issue can reach a comment.
/// Times are GitHub's `YYYY-MM-DDTHH:MM:SSZ`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// No `factory:ready` event in force among the label events read. It has no label time.
    LabelNotFound,
    /// A full page of a longer label history without the event in force. It has no label time.
    LabelHistoryIncomplete,
    /// The label in force was applied by someone who is not an instructor (`by` `None`: a
    /// deleted account).
    LabeledByNonInstructor {
        by: Option<String>,
        labeled_at: String,
    },
    /// Someone who is not an instructor edited the body or renamed the title after the label.
    EditedAfterLabel { labeled_at: String },
    /// The body's edit history is cut off, so it cannot show every edit.
    EditHistoryIncomplete { labeled_at: String },
    /// The timeline page is cut off at the label's second, so it may hide a rename in that
    /// second. The body's edit history is whole. Applying the label again puts a newer label on
    /// the newest page, which the next claim can judge.
    TimelineShortOfLabel { labeled_at: String },
}

/// Whether the factory may build the issue: an instructor applied the label in force, and
/// every edit and rename since was an instructor's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Approval {
    Approved {
        labeled_at: String,
        labeled_by: String,
    },
    Refused(Refusal),
}

/// Whether the issue text is as an instructor left it: `Unchanged`, `Changed` by someone who
/// is not an instructor, or `Unjudged` when the histories do not reach back far enough to tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextCheck {
    Unchanged,
    Changed,
    Unjudged,
}

/// The fixed reason for a refusal. Nothing from the issue, not even a login, is in it.
pub fn refusal_reason(refusal: &Refusal) -> &'static str {
    match refusal {
        Refusal::LabeledByNonInstructor { .. } => "factory:ready was not applied by an instructor",
        Refusal::EditedAfterLabel { .. } => {
            "the issue text was edited after factory:ready was applied by someone who is not an instructor"
        }
        Refusal::LabelNotFound | Refusal::LabelHistoryIncomplete => {
            "the factory could not read who applied factory:ready"
        }
        Refusal::EditHistoryIncomplete { .. } => {
            "the factory could not read the full edit history of the issue text"
        }
        Refusal::TimelineShortOfLabel { .. } => {
            "the factory could not read the issue's history back to when factory:ready was applied"
        }
    }
}

/// The comment on an issue the factory refused to build: its own fixed text, quoting nothing
/// from the issue.
pub fn unapproved_comment(refusal: &Refusal) -> String {
    let what_to_do = match refusal {
        Refusal::EditHistoryIncomplete { .. } => {
            "The issue's edit history is too long for the factory to check. File a new issue with the final text and apply `factory:ready` to it."
        }
        Refusal::TimelineShortOfLabel { .. } => {
            "The issue's recent history is too long for the factory to check from the label. Apply `factory:ready` again."
        }
        _ => {
            "Review the issue text, then apply `factory:ready` again. An instructor must apply the label, and the text must not be edited afterwards by anyone who is not an instructor."
        }
    };
    format!(
        "The factory did not start on this issue and has moved it back to `factory:proposed`.\n\nReason: {}\n\n{what_to_do}",
        refusal_reason(refusal)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const AT: &str = "2026-10-04T10:00:00Z";

    const NOT_BY_INSTRUCTOR: &str = "factory:ready was not applied by an instructor";
    const EDITED_AFTER: &str = "the issue text was edited after factory:ready was applied by someone who is not an instructor";
    const CANNOT_READ_LABEL: &str = "the factory could not read who applied factory:ready";
    const CANNOT_READ_HISTORY: &str =
        "the factory could not read the full edit history of the issue text";

    const OPENING: &str =
        "The factory did not start on this issue and has moved it back to `factory:proposed`.";
    const REVIEW_AND_RELABEL: &str = "Review the issue text, then apply `factory:ready` again. An instructor must apply the label, and the text must not be edited afterwards by anyone who is not an instructor.";
    const FILE_A_NEW_ISSUE: &str = "The issue's edit history is too long for the factory to check. File a new issue with the final text and apply `factory:ready` to it.";
    const CANNOT_READ_BACK_TO_LABEL: &str =
        "the factory could not read the issue's history back to when factory:ready was applied";
    const RELABEL: &str = "The issue's recent history is too long for the factory to check from the label. Review the issue text, then apply `factory:ready` again.";

    fn cases() -> Vec<(Refusal, &'static str, &'static str)> {
        vec![
            (
                Refusal::LabelNotFound,
                CANNOT_READ_LABEL,
                REVIEW_AND_RELABEL,
            ),
            (
                Refusal::LabelHistoryIncomplete,
                CANNOT_READ_LABEL,
                REVIEW_AND_RELABEL,
            ),
            (
                Refusal::LabeledByNonInstructor {
                    by: Some("mallory".into()),
                    labeled_at: AT.into(),
                },
                NOT_BY_INSTRUCTOR,
                REVIEW_AND_RELABEL,
            ),
            (
                Refusal::LabeledByNonInstructor {
                    by: None,
                    labeled_at: AT.into(),
                },
                NOT_BY_INSTRUCTOR,
                REVIEW_AND_RELABEL,
            ),
            (
                Refusal::EditedAfterLabel {
                    labeled_at: AT.into(),
                },
                EDITED_AFTER,
                REVIEW_AND_RELABEL,
            ),
            (
                Refusal::EditHistoryIncomplete {
                    labeled_at: AT.into(),
                },
                CANNOT_READ_HISTORY,
                FILE_A_NEW_ISSUE,
            ),
            (
                Refusal::TimelineShortOfLabel {
                    labeled_at: AT.into(),
                },
                CANNOT_READ_BACK_TO_LABEL,
                RELABEL,
            ),
        ]
    }

    // refusalReason

    #[test]
    fn refusal_reason_has_softfacs_fixed_reason() {
        for (refusal, reason, _) in cases() {
            assert_eq!(refusal_reason(&refusal), reason, "{refusal:?}");
        }
    }

    #[test]
    fn refusal_reason_gives_the_unreadable_label_history_and_the_missing_label_one_reason() {
        assert_eq!(
            refusal_reason(&Refusal::LabelNotFound),
            refusal_reason(&Refusal::LabelHistoryIncomplete)
        );
    }

    // unapprovedComment

    #[test]
    fn unapproved_comment_is_the_factorys_fixed_text() {
        for (refusal, reason, what_to_do) in cases() {
            assert_eq!(
                unapproved_comment(&refusal),
                format!("{OPENING}\n\nReason: {reason}\n\n{what_to_do}"),
                "{refusal:?}"
            );
        }
    }

    #[test]
    fn unapproved_comment_is_softfacs_literal_comment_for_an_edit_after_the_label() {
        assert_eq!(
            unapproved_comment(&Refusal::EditedAfterLabel {
                labeled_at: AT.into()
            }),
            "The factory did not start on this issue and has moved it back to `factory:proposed`.\n\nReason: the issue text was edited after factory:ready was applied by someone who is not an instructor\n\nReview the issue text, then apply `factory:ready` again. An instructor must apply the label, and the text must not be edited afterwards by anyone who is not an instructor."
        );
    }

    #[test]
    fn unapproved_comment_is_softfacs_literal_comment_for_a_history_that_is_too_long() {
        assert_eq!(
            unapproved_comment(&Refusal::EditHistoryIncomplete {
                labeled_at: AT.into()
            }),
            "The factory did not start on this issue and has moved it back to `factory:proposed`.\n\nReason: the factory could not read the full edit history of the issue text\n\nThe issue's edit history is too long for the factory to check. File a new issue with the final text and apply `factory:ready` to it."
        );
    }

    #[test]
    fn unapproved_comment_says_to_relabel_when_the_timeline_stops_short_of_the_label() {
        assert_eq!(
            unapproved_comment(&Refusal::TimelineShortOfLabel {
                labeled_at: AT.into()
            }),
            "The factory did not start on this issue and has moved it back to `factory:proposed`.\n\nReason: the factory could not read the issue's history back to when factory:ready was applied\n\nThe issue's recent history is too long for the factory to check from the label. Review the issue text, then apply `factory:ready` again."
        );
    }

    #[test]
    fn unapproved_comment_quotes_nothing_from_the_issue_or_the_login() {
        let text = unapproved_comment(&Refusal::LabeledByNonInstructor {
            by: Some("mallory".into()),
            labeled_at: AT.into(),
        });
        assert!(!text.contains("mallory"));
        assert!(!text.contains(AT));
        let text = unapproved_comment(&Refusal::TimelineShortOfLabel {
            labeled_at: AT.into(),
        });
        assert!(!text.contains(AT));
    }
}

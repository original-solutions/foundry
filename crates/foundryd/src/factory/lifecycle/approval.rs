//! Whether an issue is approved for the factory. Pure.

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
    fn unapproved_comment_quotes_nothing_from_the_issue_or_the_login() {
        let text = unapproved_comment(&Refusal::LabeledByNonInstructor {
            by: Some("mallory".into()),
            labeled_at: AT.into(),
        });
        assert!(!text.contains("mallory"));
        assert!(!text.contains(AT));
    }
}

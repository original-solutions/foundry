//! An issue's `Depends on` section and whether it is met. Pure.

#[cfg(test)]
mod tests {
    use super::*;

    fn same(number: u64) -> IssueRef {
        IssueRef { repo: None, number }
    }

    fn cross(repo: &str, number: u64) -> IssueRef {
        IssueRef {
            repo: Some(repo.into()),
            number,
        }
    }

    #[test]
    fn depends_on_reads_same_repo_references() {
        let body = "Intro\n\n## Depends on\n\n- #13\n- #15 until it lands\n";
        assert_eq!(blockers(body), vec![same(13), same(15)]);
    }

    #[test]
    fn depends_on_reads_cross_repo_references() {
        let body = "## Depends on\nacme/widgets#7, my-org/some.repo_x#42\n";
        assert_eq!(
            blockers(body),
            vec![cross("acme/widgets", 7), cross("my-org/some.repo_x", 42)]
        );
    }

    #[test]
    fn depends_on_reads_both_kinds_in_order_and_once() {
        let body = "## Depends on\n#3 and acme/widgets#7 and #3 again, #9\n";
        assert_eq!(
            blockers(body),
            vec![same(3), cross("acme/widgets", 7), same(9)]
        );
    }

    #[test]
    fn depends_on_does_not_count_the_number_of_a_cross_repo_reference_twice() {
        assert_eq!(
            blockers("## Depends on\nacme/widgets#7\n"),
            vec![cross("acme/widgets", 7)]
        );
    }

    #[test]
    fn depends_on_none_or_empty_or_missing_names_nothing() {
        assert_eq!(blockers("## Depends on\n\nNone\n"), vec![]);
        assert_eq!(blockers("## Depends on\nnone.\n## Next\n#4\n"), vec![]);
        assert_eq!(blockers("## Depends on\n"), vec![]);
        assert_eq!(blockers("## Depends on\n\n## Fix\n#4\n"), vec![]);
        assert_eq!(blockers("## Cause\nSee #4\n"), vec![]);
        assert_eq!(blockers(""), vec![]);
    }

    #[test]
    fn depends_on_ignores_text_after_the_next_heading() {
        let body = "## Depends on\n#13\n### Notes\n#99\n## Fix\nacme/widgets#98\n";
        assert_eq!(blockers(body), vec![same(13)]);
    }

    #[test]
    fn depends_on_ignores_references_before_the_section() {
        assert_eq!(
            blockers("## Cause\nSee #5\n\n## Depends on\n#13\n"),
            vec![same(13)]
        );
    }

    #[test]
    fn depends_on_heading_is_case_insensitive_and_allows_trailing_spaces() {
        assert_eq!(blockers("## depends ON  \r\n#13\r\n"), vec![same(13)]);
    }

    #[test]
    fn depends_on_needs_digits_after_the_hash() {
        assert_eq!(blockers("## Depends on\n#abc # 4 #\n"), vec![]);
    }

    #[test]
    fn depends_on_skips_a_hash_that_word_characters_run_into() {
        assert_eq!(
            blockers("## Depends on\nsee page#12 and /widgets#7, then #3\n"),
            vec![same(3)]
        );
    }

    #[test]
    fn depends_on_ends_at_a_heading_line_with_nothing_after_the_hashes() {
        assert_eq!(blockers("## Depends on\n#3\n##\n#4\n"), vec![same(3)]);
    }

    #[test]
    fn depends_on_heading_needs_two_hashes_and_a_space() {
        assert_eq!(blockers("##Depends on\n#3\n"), vec![]);
        assert_eq!(blockers("### Depends on\n#3\n"), vec![]);
        assert_eq!(blockers("# Depends on\n#3\n"), vec![]);
    }

    #[test]
    fn depends_on_skips_an_owner_or_repo_made_only_of_dots_which_would_leave_the_api_path() {
        assert_eq!(
            blockers("## Depends on\n../..#3 ./widgets#4 acme/..#5 acme/.#6 ...//x#8\n"),
            vec![]
        );
        assert_eq!(
            blockers("## Depends on\nacme/.github#7 a.b/c..d#9\n"),
            vec![cross("acme/.github", 7), cross("a.b/c..d", 9)]
        );
    }

    #[test]
    fn depends_on_skips_a_number_too_large_to_be_exact() {
        assert_eq!(
            blockers("## Depends on\n#9007199254740992 #99999999999999999999999 #4\n"),
            vec![same(4)]
        );
    }
}

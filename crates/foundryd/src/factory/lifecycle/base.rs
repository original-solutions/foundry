//! The branch a run starts from: an issue's `## Base` or the project's base. Pure.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_is_null_with_no_section() {
        assert_eq!(of(""), None);
        assert_eq!(of("## Outcome\nShip it.\n## Depends on\nNone.\n"), None);
    }

    #[test]
    fn base_is_null_for_an_empty_section_or_none() {
        assert_eq!(of("## Base\n\n## Notes\nfeature/x\n"), None);
        assert_eq!(of("## Base\n"), None);
        assert_eq!(of("## Base\n\nNone.\n"), None);
        assert_eq!(of("## Base\nnone.\n## Notes\n"), None);
        assert_eq!(of("## Base\n  NONE.  \n"), None);
    }

    #[test]
    fn base_reads_the_branch_name() {
        assert_eq!(
            of("## Outcome\nx\n\n## Base\n\nfeature/x\n\n## Notes\ny\n").as_deref(),
            Some("feature/x")
        );
        assert_eq!(
            of("## Base\r\n  feature/x  \r\n").as_deref(),
            Some("feature/x")
        );
    }

    #[test]
    fn base_takes_the_first_non_blank_line_and_ignores_the_prose_after_it() {
        let body = "## Base\n\nfeature/x\nThe group of issues lands here first.\nThen main.\n";
        assert_eq!(of(body).as_deref(), Some("feature/x"));
    }

    #[test]
    fn base_as_the_last_heading_runs_to_the_end_of_the_body() {
        assert_eq!(
            of("## Outcome\nx\n## Depends on\nNone.\n## Base\nrelease/2\n").as_deref(),
            Some("release/2")
        );
    }

    #[test]
    fn base_treats_a_hash_reference_inside_the_section_as_text_not_a_heading() {
        assert_eq!(of("## Base\n#12\n").as_deref(), Some("#12"));
        assert_eq!(
            of("## Base\n\nfeature/x\n#12 is the parent\n## Notes\n").as_deref(),
            Some("feature/x")
        );
    }
}

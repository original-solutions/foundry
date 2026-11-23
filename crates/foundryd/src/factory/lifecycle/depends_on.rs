//! An issue's `Depends on` section and whether it is met. Pure.
//!
//! Text parsing of the issue body, no `gh` call. Ported from the runner's
//! `src/domain/dependsOn.ts`.

/// An issue named in the `## Depends on` section of an issue body.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct IssueRef {
    /// `owner/name` when the text names a repo; `None` means the repo of the issue it was
    /// written in.
    pub repo: Option<String>,
    pub number: u64,
}

/// The largest number taken, as the runner takes it: past 2^53 a number is not exact there.
const MAX_NUMBER: u64 = (1 << 53) - 1;

fn is_repo_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
}

fn is_dots(segment: &str) -> bool {
    !segment.is_empty() && segment.bytes().all(|b| b == b'.')
}

/// A markdown heading: `#`s then a space, a tab or the end of the line. `#13` is a reference.
fn is_heading(line: &str) -> bool {
    let rest = line.trim_start_matches('#');
    rest.len() < line.len() && (rest.is_empty() || rest.starts_with([' ', '\t']))
}

/// The lines of the issue body's `## <name>` section (`name` in lower case): after its
/// heading, up to the next heading line. Empty when the body has no such section.
pub fn section_lines<'a>(body: &'a str, name: &str) -> Vec<&'a str> {
    let mut lines = Vec::new();
    let mut in_section = false;
    for line in body.lines() {
        if in_section {
            if is_heading(line) {
                break;
            }
            lines.push(line);
        } else {
            in_section = (line.starts_with("## ") || line.starts_with("##\t"))
                && line[3..].trim().to_lowercase() == name;
        }
    }
    lines
}

/// The issues named in the `## Depends on` section of an issue body, in order of first
/// appearance, each once. The section runs from its heading to the next heading line. It names
/// `#<n>` and `<owner>/<repo>#<n>`; "None", an empty section and no section name nothing.
pub fn blockers(body: &str) -> Vec<IssueRef> {
    let mut found: Vec<IssueRef> = Vec::new();
    for line in section_lines(body, "depends on") {
        let bytes = line.as_bytes();
        for at in 0..bytes.len() {
            if bytes[at] != b'#' {
                continue;
            }
            let mut end = at + 1;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            if end == at + 1 {
                continue;
            }
            let Some(number) = line[at + 1..end]
                .parse::<u64>()
                .ok()
                .filter(|n| *n <= MAX_NUMBER)
            else {
                continue;
            };
            let mut repo_start = at;
            while repo_start > 0 && is_repo_byte(bytes[repo_start - 1]) {
                repo_start -= 1;
            }
            let repo = if repo_start < at && repo_start > 0 && bytes[repo_start - 1] == b'/' {
                let mut owner_start = repo_start - 1;
                while owner_start > 0 && is_repo_byte(bytes[owner_start - 1]) {
                    owner_start -= 1;
                }
                if owner_start == repo_start - 1 {
                    continue;
                }
                let owner = &line[owner_start..repo_start - 1];
                let name = &line[repo_start..at];
                // `.` and `..` would step out of `repos/<owner>/<name>` in the API path: not a repo.
                if is_dots(owner) || is_dots(name) {
                    continue;
                }
                Some(format!("{owner}/{name}"))
            } else if repo_start < at {
                // Word characters run straight into the `#`, as in `page#12`: not an issue.
                continue;
            } else {
                None
            };
            let issue = IssueRef { repo, number };
            if !found.contains(&issue) {
                found.push(issue);
            }
        }
    }
    found
}

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

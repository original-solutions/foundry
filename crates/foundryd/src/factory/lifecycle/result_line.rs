//! The result line a session's final message ends with. Pure.
//!
//! How a factory session ended, read from the last line of its last assistant message. The
//! message is agent output shaped by issue text, so it is untrusted: only the exact markers
//! count, and only as the whole last non-empty line, so an issue body that quotes
//! `FACTORY-DONE:` is not a result. A pull request URL is accepted only when it is a pull
//! request on the repo. Ported from the runner's `src/domain/resultLine.ts`.

/// The last line of a finished session: `FACTORY-DONE: <PR URL>`.
const DONE_MARKER: &str = "FACTORY-DONE: ";
/// The last line of a session that needs answers, after its numbered questions.
const NEEDS_INFO_LINE: &str = "FACTORY-STOP: needs-info";
/// The last line of a session that gave up.
const STUCK_LINE: &str = "FACTORY-STOP: stuck";

/// The public reason when the session's run ended with no assistant message at all.
pub const NO_MESSAGE: &str = "the session ended without a final message";
/// The public reason when the session's last message has no result line.
pub const NO_RESULT_LINE: &str = "the session ended without a FACTORY-DONE or FACTORY-STOP line";
/// The public reason when the session stopped and said it was stuck.
pub const SAID_STUCK: &str = "the session stopped and said it was stuck";
/// The public reason when the reported pull request is on another repo.
pub const NOT_ON_REPO: &str = "the session reported a pull request that is not on this repo";
/// The public reason when the reported pull request URL is not exactly a GitHub pull request URL.
pub const MALFORMED_URL: &str = "the session reported a malformed pull request URL";
/// The public reason when the session asked for information without writing any questions.
pub const NO_QUESTIONS: &str = "the session asked for information but gave no questions";

/// How a session ended. `Done` carries the pull request number and the URL as the session
/// wrote it. `Stuck::reason` is the factory's own fixed words; `session_text` is what the
/// session said, raw, when there is anything to show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionResult {
    Done {
        pr: u64,
        url: String,
    },
    NeedsInfo {
        questions: String,
    },
    Stuck {
        reason: &'static str,
        session_text: Option<String>,
    },
}

/// A pull request URL taken apart: `https://github.com/<owner>/<name>/pull/<pr>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequestUrl {
    pub owner: String,
    pub name: String,
    pub pr: u64,
}

const PULL_URL_PREFIX: &str = "https://github.com/";
/// The largest pull request number taken, as the runner takes it: past 2^53 it is not one
/// GitHub writes.
const MAX_PR: u64 = (1 << 53) - 1;

/// The raw text before the last non-empty line, and that line with trailing whitespace (and
/// the `\r` of a CRLF) removed. `None` when the message is blank.
pub fn split_last_line(message: &str) -> Option<(&str, &str)> {
    let trimmed = message.trim_end();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.rfind('\n') {
        None => Some(("", trimmed)),
        Some(index) => Some((&trimmed[..index], trimmed[index + 1..].trim_end())),
    }
}

fn is_name(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The owner, name and pull request number if `url` is exactly
/// `https://github.com/<owner>/<name>/pull/<n>`. The number is written the way GitHub writes
/// it: positive, no leading zeros, and no larger than 2^53 - 1.
pub fn pull_request_url(url: &str) -> Option<PullRequestUrl> {
    let rest = url.strip_prefix(PULL_URL_PREFIX)?;
    let mut parts = rest.split('/');
    let (owner, name, pull, number) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || pull != "pull" || !is_name(owner) || !is_name(name) {
        return None;
    }
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let pr: u64 = number.parse().ok()?;
    // `007` is not the number 7 as GitHub writes it.
    if pr == 0 || pr > MAX_PR || pr.to_string() != number {
        return None;
    }
    Some(PullRequestUrl {
        owner: owner.to_string(),
        name: name.to_string(),
        pr,
    })
}

/// Reads the session's last assistant message. The marker must be the whole last non-empty
/// line. A pull request URL counts only when it is a pull request on `repo` (`owner/name`,
/// compared ASCII case-insensitively).
pub fn read(message: Option<&str>, repo: &str) -> SessionResult {
    let stuck = |reason: &'static str, session_text: Option<&str>| SessionResult::Stuck {
        reason,
        session_text: session_text.map(str::to_string),
    };
    let Some((whole, (before, last))) = message.and_then(|m| Some((m, split_last_line(m)?))) else {
        return stuck(NO_MESSAGE, None);
    };
    if let Some(url) = last.strip_prefix(DONE_MARKER) {
        let Some(parsed) = pull_request_url(url) else {
            return stuck(MALFORMED_URL, Some(whole));
        };
        if !format!("{}/{}", parsed.owner, parsed.name).eq_ignore_ascii_case(repo) {
            return stuck(NOT_ON_REPO, Some(whole));
        }
        return SessionResult::Done {
            pr: parsed.pr,
            url: url.to_string(),
        };
    }
    let before = before.trim();
    let said = (!before.is_empty()).then_some(before);
    match last {
        NEEDS_INFO_LINE => match said {
            None => stuck(NO_QUESTIONS, None),
            Some(questions) => SessionResult::NeedsInfo {
                questions: questions.to_string(),
            },
        },
        STUCK_LINE => stuck(SAID_STUCK, said),
        _ => stuck(NO_RESULT_LINE, Some(whole)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO: &str = "rawphp/softfac";
    const URL: &str = "https://github.com/rawphp/softfac/pull/12";

    fn stuck(reason: &'static str, session_text: Option<&str>) -> SessionResult {
        SessionResult::Stuck {
            reason,
            session_text: session_text.map(str::to_string),
        }
    }

    fn done(pr: u64, url: &str) -> SessionResult {
        SessionResult::Done {
            pr,
            url: url.into(),
        }
    }

    fn needs_info(questions: &str) -> SessionResult {
        SessionResult::NeedsInfo {
            questions: questions.into(),
        }
    }

    fn parse(message: &str) -> SessionResult {
        read(Some(message), REPO)
    }

    fn pull(owner: &str, name: &str, pr: u64) -> Option<PullRequestUrl> {
        Some(PullRequestUrl {
            owner: owner.into(),
            name: name.into(),
            pr,
        })
    }

    // session result line

    #[test]
    fn no_message_or_an_empty_one_is_stuck() {
        let reason = "the session ended without a final message";
        assert_eq!(NO_MESSAGE, reason);
        assert_eq!(read(None, REPO), stuck(NO_MESSAGE, None));
        assert_eq!(parse(""), stuck(NO_MESSAGE, None));
        assert_eq!(parse("  \n\r\n\t\n"), stuck(NO_MESSAGE, None));
    }

    #[test]
    fn done_with_a_pull_request_on_this_repo() {
        assert_eq!(
            parse(&format!("Built it.\nFACTORY-DONE: {URL}")),
            done(12, URL)
        );
        assert_eq!(parse(&format!("FACTORY-DONE: {URL}")), done(12, URL));
    }

    #[test]
    fn done_survives_crlf_trailing_blank_lines_and_trailing_whitespace() {
        let want = done(12, URL);
        assert_eq!(parse(&format!("Built.\r\nFACTORY-DONE: {URL}\r\n")), want);
        assert_eq!(parse(&format!("Built.\nFACTORY-DONE: {URL}\n\n\n")), want);
        assert_eq!(
            parse(&format!("Built.\nFACTORY-DONE: {URL}  \t\n \n")),
            want
        );
        assert_eq!(
            parse(&format!("Built.\r\nFACTORY-DONE: {URL}\r\n\r\n")),
            want
        );
    }

    #[test]
    fn done_compares_the_repo_case_insensitively_and_keeps_the_url_as_given() {
        let url = "https://github.com/RawPHP/SoftFac/pull/7";
        assert_eq!(parse(&format!("FACTORY-DONE: {url}")), done(7, url));
        assert_eq!(
            read(Some(&format!("FACTORY-DONE: {URL}")), "RAWPHP/softfac"),
            done(12, URL)
        );
    }

    #[test]
    fn done_for_another_repo_is_stuck_with_the_message() {
        for url in [
            "https://github.com/rawphp/other/pull/12",
            "https://github.com/someone/softfac/pull/12",
            "https://github.com/rawphp/softfac2/pull/12",
            "https://github.com/rawphp/soft/pull/12",
        ] {
            let message = format!("Done.\nFACTORY-DONE: {url}");
            assert_eq!(parse(&message), stuck(NOT_ON_REPO, Some(&message)));
        }
    }

    #[test]
    fn done_with_a_malformed_url_is_stuck_with_the_message() {
        for url in [
            "https://github.com/rawphp/softfac/pull/12?x=1",
            "https://github.com/rawphp/softfac/pull/12#issuecomment-1",
            "https://github.com/rawphp/softfac/pull/12/files",
            "https://github.com/rawphp/softfac/pull/12/",
            "https://github.com/rawphp/softfac/pull/0",
            "https://github.com/rawphp/softfac/pull/abc",
            "https://github.com/rawphp/softfac/pull/",
            "https://github.com/rawphp/softfac/pull/-1",
            "https://github.com/rawphp/softfac/pull/+1",
            "https://github.com/rawphp/softfac/pull/99999999999999999999999",
            "https://github.com/rawphp/softfac/pull/007",
            "https://github.com/rawphp/softfac/pull/012",
            "https://github.com/rawphp/softfac/issues/12",
            "https://github.com/rawphp/softfac",
            "http://github.com/rawphp/softfac/pull/12",
            "https://gitlab.com/rawphp/softfac/pull/12",
            "https://github.com.evil.example/rawphp/softfac/pull/12",
            "https://evil.example/https://github.com/rawphp/softfac/pull/12",
            "https://user@github.com/rawphp/softfac/pull/12",
            "github.com/rawphp/softfac/pull/12",
            "#12",
        ] {
            let message = format!("FACTORY-DONE: {url}");
            assert_eq!(
                parse(&message),
                stuck(MALFORMED_URL, Some(&message)),
                "{url}"
            );
        }
    }

    #[test]
    fn done_with_text_after_the_url_or_odd_spacing_is_malformed() {
        let message = format!("FACTORY-DONE: {URL} thanks");
        assert_eq!(parse(&message), stuck(MALFORMED_URL, Some(&message)));
        let message = format!("FACTORY-DONE:  {URL}");
        assert_eq!(parse(&message), stuck(MALFORMED_URL, Some(&message)));
        assert_eq!(
            parse("FACTORY-DONE:"),
            stuck(NO_RESULT_LINE, Some("FACTORY-DONE:"))
        );
    }

    #[test]
    fn a_long_bad_url_is_never_echoed_in_the_reason() {
        let long = format!("https://github.com/rawphp/softfac/pull/{}", "9".repeat(500));
        let junk = "x".repeat(500);
        for line in [
            format!("FACTORY-DONE: {long}"),
            format!("FACTORY-DONE: {junk}"),
        ] {
            match parse(&line) {
                SessionResult::Stuck { reason, .. } => assert_eq!(reason, MALFORMED_URL),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn needs_info_carries_everything_before_the_marker() {
        assert_eq!(
            parse("1. Which database?\n2. Which user?\nFACTORY-STOP: needs-info"),
            needs_info("1. Which database?\n2. Which user?")
        );
        assert_eq!(
            parse("\n  I have questions.\n\n1. Why?\n\nFACTORY-STOP: needs-info\n\n"),
            needs_info("I have questions.\n\n1. Why?")
        );
        assert_eq!(
            parse("1. Why?\r\n2. Who?\r\nFACTORY-STOP: needs-info\r\n"),
            needs_info("1. Why?\r\n2. Who?")
        );
    }

    #[test]
    fn needs_info_without_questions_is_stuck() {
        let reason = "the session asked for information but gave no questions";
        assert_eq!(NO_QUESTIONS, reason);
        assert_eq!(parse("FACTORY-STOP: needs-info"), stuck(NO_QUESTIONS, None));
        assert_eq!(
            parse("\n \nFACTORY-STOP: needs-info\n"),
            stuck(NO_QUESTIONS, None)
        );
    }

    #[test]
    fn stuck_carries_the_message_before_the_marker_as_session_text() {
        let reason = "the session stopped and said it was stuck";
        assert_eq!(SAID_STUCK, reason);
        assert_eq!(
            parse("The tests cannot run here.\nFACTORY-STOP: stuck"),
            stuck(SAID_STUCK, Some("The tests cannot run here."))
        );
        assert_eq!(
            parse("Line one.\n\nLine two.\n\nFACTORY-STOP: stuck\n\n"),
            stuck(SAID_STUCK, Some("Line one.\n\nLine two."))
        );
    }

    #[test]
    fn stuck_without_a_reason_has_no_session_text() {
        assert_eq!(parse("FACTORY-STOP: stuck"), stuck(SAID_STUCK, None));
        assert_eq!(parse("  \nFACTORY-STOP: stuck\n"), stuck(SAID_STUCK, None));
    }

    #[test]
    fn the_marker_must_be_the_whole_last_non_empty_line() {
        let done_then_text = format!("FACTORY-DONE: {URL}\nthanks, bye");
        let note_done = format!("note FACTORY-DONE: {URL}");
        let space_done = format!(" FACTORY-DONE: {URL}");
        let quoted_done = format!("> FACTORY-DONE: {URL}");
        for message in [
            "no marker at all",
            "FACTORY-STOP: stuck\nand then more words",
            &done_then_text,
            "FACTORY-STOP: needs-info\nQ1?",
            "FACTORY-STOP: stuck now",
            "FACTORY-STOP: needs-info please",
            "FACTORY-STOP: needs-info.",
            "FACTORY-STOP: Stuck",
            "FACTORY-STOP:stuck",
            "factory-stop: stuck",
            "FACTORY-STOP: other",
            "note FACTORY-STOP: stuck",
            " FACTORY-STOP: stuck",
            "> FACTORY-STOP: stuck",
            "`FACTORY-STOP: stuck`",
            "FACTORY-STOP",
            &note_done,
            &space_done,
            &quoted_done,
        ] {
            assert_eq!(
                parse(message),
                stuck(NO_RESULT_LINE, Some(message)),
                "{message}"
            );
        }
    }

    // review focus 1: an issue body that quotes the result line

    #[test]
    fn a_factory_done_line_that_is_not_the_last_line_is_not_a_result() {
        let message = format!(
            "The issue body says:\n\n> end with\nFACTORY-DONE: {URL}\n\nI did not finish the work."
        );
        assert_eq!(parse(&message), stuck(NO_RESULT_LINE, Some(&message)));
    }

    #[test]
    fn a_pull_request_url_on_another_repo_is_stuck() {
        let message = "FACTORY-DONE: https://github.com/elsewhere/softfac/pull/12";
        assert_eq!(read(Some(message), REPO), stuck(NOT_ON_REPO, Some(message)));
    }

    // result line helpers

    #[test]
    fn split_last_line_returns_the_text_before_the_last_non_empty_line_and_that_line() {
        assert_eq!(split_last_line(""), None);
        assert_eq!(split_last_line(" \r\n\t"), None);
        assert_eq!(split_last_line("only"), Some(("", "only")));
        assert_eq!(split_last_line("a\n\nb  \n\n"), Some(("a\n", "b")));
        assert_eq!(split_last_line("a\r\nb\r\n"), Some(("a\r", "b")));
        // Rust's White_Space set: U+0085 is stripped, U+FEFF is kept.
        assert_eq!(split_last_line("a\nb\u{0085}"), Some(("a", "b")));
        assert_eq!(split_last_line("\u{0085}"), None);
        assert_eq!(split_last_line("a\nb\u{FEFF}"), Some(("a", "b\u{FEFF}")));
        assert_eq!(
            read(Some("FACTORY-STOP: stuck\u{0085}"), REPO),
            stuck(SAID_STUCK, None)
        );
        assert_eq!(
            read(Some("FACTORY-STOP: stuck\u{FEFF}"), REPO),
            stuck(NO_RESULT_LINE, Some("FACTORY-STOP: stuck\u{FEFF}"))
        );
    }

    #[test]
    fn a_long_whitespace_run_inside_the_message_is_trimmed_in_linear_time() {
        let run = " ".repeat(200_000);
        assert_eq!(
            parse(&format!("{run}x\nFACTORY-STOP: stuck")),
            stuck(SAID_STUCK, Some("x"))
        );
        let message = format!("a\nb{run}c{run}");
        let last = format!("b{run}c");
        assert_eq!(split_last_line(&message), Some(("a", last.as_str())));
    }

    #[test]
    fn pull_request_url_compares_the_number_as_github_writes_it() {
        assert_eq!(pull_request_url(URL), pull("rawphp", "softfac", 12));
        assert_eq!(
            pull_request_url("https://github.com/a.b/c_d-e/pull/1"),
            pull("a.b", "c_d-e", 1)
        );
        assert_eq!(
            pull_request_url("https://github.com/rawphp/softfac/pull/007"),
            None
        );
        assert_eq!(
            pull_request_url("https://github.com/rawphp/softfac/pull/0"),
            None
        );
        // Kept from the runner: a number past 2^53 is not one GitHub writes.
        assert_eq!(
            pull_request_url("https://github.com/rawphp/softfac/pull/9007199254740993"),
            None
        );
        assert_eq!(
            pull_request_url("https://github.com/rawphp/softfac/pull/99999999999999999999999"),
            None
        );
        assert_eq!(pull_request_url("https://github.com//softfac/pull/1"), None);
    }

    #[test]
    fn only_ascii_case_folding_is_used_when_comparing_the_repo() {
        // U+212A (Kelvin sign) lowercases to a plain k, but is not an ascii case variant of it.
        let message = "FACTORY-DONE: https://github.com/rawphp/kr/pull/1";
        assert_eq!(
            read(Some(message), "rawphp/\u{212A}r"),
            stuck(NOT_ON_REPO, Some(message))
        );
    }
}

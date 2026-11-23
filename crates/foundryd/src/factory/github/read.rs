//! Reads from GitHub: issues, comments, labels and pull requests.
//!
//! Each read runs `gh` through [`Tracker::gh`] and reads its answer, so the real `gh` and the
//! fake are read alike. Ported from the reading half of the runner's `src/tracker/github.ts`,
//! and from `src/tracker/approvalHistory.ts`, the pure half of the approval rule.

use serde::de::DeserializeOwned;

use super::gh::{invalid_argument, one_line, parse_error};
use super::{Tracker, TrackerError};
use crate::factory::lifecycle::State;
use crate::factory::lifecycle::approval::{Approval, TextCheck};

// ---------------------------------------------------------------------------------------------
// Shared: running a read and decoding its answer.
// ---------------------------------------------------------------------------------------------

/// Runs a read and decodes its stdout as JSON of the shape `T`; serde's complaint joins
/// `expected`.
pub(crate) fn gh_json<T, G>(gh: &G, args: &[String], expected: &str) -> Result<T, TrackerError>
where
    T: DeserializeOwned,
    G: Tracker + ?Sized,
{
    let stdout = gh.gh(args)?;
    decode(args, expected, &stdout)
}

/// Decodes `text` as JSON of the shape `T`, or the parse error for the call with `args`.
pub(crate) fn decode<T: DeserializeOwned>(
    args: &[String],
    expected: &str,
    text: &str,
) -> Result<T, TrackerError> {
    serde_json::from_str(text).map_err(|error| {
        let why = error.to_string().split_whitespace().collect::<Vec<_>>().join(" ");
        parse_error(args, &format!("{expected} ({why})"))
    })
}

/// Runs `gh api --paginate` and collects the rows of every JSON array in stdout: gh prints
/// either one merged array or one array per page.
pub(crate) fn gh_pages<T, G>(
    gh: &G,
    args: &[String],
    expected: &str,
) -> Result<Vec<T>, TrackerError>
where
    T: DeserializeOwned,
    G: Tracker + ?Sized,
{
    let stdout = gh.gh(args)?;
    let pages = split_json_values(&stdout);
    if pages.is_empty() {
        return Err(parse_error(args, &format!("{expected} (output was empty)")));
    }
    let mut rows = Vec::new();
    for page in pages {
        rows.extend(decode::<Vec<T>>(args, expected, page)?);
    }
    Ok(rows)
}

fn is_json_space(c: u8) -> bool {
    matches!(c, b' ' | b'\n' | b'\r' | b'\t')
}

/// Splits text holding several top-level JSON values (`gh --paginate` prints one array per
/// page) into the text of each. Brackets and quotes inside strings do not count. A value that
/// is cut short comes back as is, for the decoder to refuse.
pub(crate) fn split_json_values(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut values = Vec::new();
    let mut i = 0;
    loop {
        while i < bytes.len() && is_json_space(bytes[i]) {
            i += 1;
        }
        if i >= bytes.len() {
            return values;
        }
        let start = i;
        let mut depth = 0i64;
        let mut in_string = false;
        while i < bytes.len() {
            let c = bytes[i];
            if in_string {
                if c == b'\\' {
                    i += 1;
                } else if c == b'"' {
                    in_string = false;
                    if depth == 0 {
                        i += 1;
                        break;
                    }
                }
            } else if c == b'"' {
                in_string = true;
            } else if c == b'[' || c == b'{' {
                depth += 1;
            } else if c == b']' || c == b'}' {
                depth -= 1;
                if depth <= 0 {
                    i += 1;
                    break;
                }
            } else if depth == 0 && is_json_space(c) {
                break;
            }
            i += 1;
        }
        let end = i.min(bytes.len());
        // Every boundary above is an ASCII byte, so the slice is on character boundaries.
        values.push(&text[start..end]);
    }
}

/// The arguments of a GraphQL `query` about issue or pull request `number` of `repo`, or the
/// parse error for a repo not written as `owner/name`.
pub(crate) fn issue_query_args(
    repo: &str,
    query: &str,
    number: u64,
) -> Result<Vec<String>, TrackerError> {
    let Some((owner, name)) = repo.split_once('/') else {
        return Err(parse_error(
            &["api".into(), "graphql".into()],
            &format!("a repo written as owner/name, got `{repo}`"),
        ));
    };
    Ok(vec![
        "api".into(),
        "graphql".into(),
        "-f".into(),
        format!("query={query}"),
        "-f".into(),
        format!("owner={owner}"),
        "-f".into(),
        format!("name={name}"),
        "-F".into(),
        format!("number={number}"),
    ])
}

/// `text` quoted for an error message, on one line.
pub(crate) fn quoted(text: &str) -> String {
    format!("\"{}\"", one_line(text))
}

/// Whether `text` is GitHub's uniform UTC form `YYYY-MM-DDTHH:MM:SSZ`, so ordering the text
/// orders the times.
pub fn is_utc_second(text: &str) -> bool {
    let b = text.as_bytes();
    let digits = |range: std::ops::Range<usize>| b[range].iter().all(u8::is_ascii_digit);
    b.len() == 20
        && digits(0..4)
        && b[4] == b'-'
        && digits(5..7)
        && b[7] == b'-'
        && digits(8..10)
        && b[10] == b'T'
        && digits(11..13)
        && b[13] == b':'
        && digits(14..16)
        && b[16] == b':'
        && digits(17..19)
        && b[19] == b'Z'
}

/// Whether an account's login is an instructor's, in ASCII case. A deleted account (none)
/// never is.
pub fn is_instructor(login: Option<&str>, instructors: &[String]) -> bool {
    login.is_some_and(|login| {
        instructors
            .iter()
            .any(|name| name.eq_ignore_ascii_case(login))
    })
}

// ---------------------------------------------------------------------------------------------
// Part A: listings, one issue, blockers, pull requests, checks, branches and files.
// ---------------------------------------------------------------------------------------------

/// How many issues one listing of a label reads; past this the oldest are left out.
pub const LIST_LIMIT: usize = 200;

/// The most a file read through the contents API may be. A project config is a few lines.
pub const MAX_FILE_BYTES: u64 = 64 * 1024;

/// The most issues a `## Depends on` section may name. Each costs a `gh` call on every check,
/// so a longer list is refused rather than read.
pub const MAX_DEPENDS_ON: usize = 100;

/// An open issue in a listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueSummary {
    pub number: u64,
    pub title: String,
    pub assignees: Vec<String>,
    /// The label names.
    pub labels: Vec<String>,
}

/// `cut` is set when the listing had more than [`LIST_LIMIT`] items.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listing<T> {
    pub items: Vec<T>,
    pub cut: bool,
}

/// One issue as `gh issue view` reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueDetail {
    pub number: u64,
    pub title: String,
    /// Empty for an issue with no description.
    pub body: String,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    /// The author's login; `None` when GitHub gives none (a deleted account).
    pub author: Option<String>,
    /// The state of the one `factory:*` label; `None` for none, several or an unknown one.
    pub state: Option<State>,
    /// GitHub has the issue closed.
    pub closed: bool,
}

/// An open issue that blocks another one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blocker {
    /// `owner/name` of the repo the blocking issue is in.
    pub repo: String,
    pub number: u64,
}

/// Whether GitHub can merge a pull request into its base. GitHub computes it lazily: the read
/// that answers `Unknown` starts the computation, and a later read usually has the answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mergeable {
    Mergeable,
    Conflicting,
    Unknown,
}

/// One pull request, as `gh pr view` reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub is_draft: bool,
    /// Neither merged nor closed.
    pub open: bool,
    /// Merged into its base.
    pub merged: bool,
    /// The branch the pull request comes from.
    pub head_ref: String,
    /// The head is in a fork, not in this repo.
    pub cross_repository: bool,
    /// The branch the pull request merges into.
    pub base_ref: String,
    /// The commit the head branch points at, as GitHub reports it.
    pub head_sha: String,
    pub mergeable: Mergeable,
    /// When it was opened, as GitHub writes it: `YYYY-MM-DDTHH:MM:SSZ`.
    pub created_at: String,
}

/// An open pull request as a listing gives it. Forks and other bases are listed too: the
/// caller decides which is the factory's own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenPullRequest {
    pub number: u64,
    pub url: String,
    pub head_ref: String,
    pub base_ref: String,
    pub cross_repository: bool,
    /// The login that opened it; `None` when GitHub names none.
    pub author: Option<String>,
}

/// How one required check stands on a commit: `Passed` when every run completed as `success`
/// or `neutral`, `Failed` when one completed as anything else, `Pending` when a run has not
/// completed, and `Missing` when no run of that name has a result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckState {
    Passed,
    Failed,
    Pending,
    Missing,
}

/// One required check as read for a commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckStatus {
    /// The name that was asked for.
    pub name: String,
    pub state: CheckState,
    /// What GitHub says about the run that decided `state`: its conclusion when it completed,
    /// its status when it did not, and empty for a check with no run at all.
    pub conclusion: String,
    /// The run's page on GitHub, when it has one.
    pub url: Option<String>,
}

/// The open issues with `state`'s label.
pub fn list_issues<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    state: State,
) -> Result<Listing<IssueSummary>, TrackerError> {
    let _ = (gh, repo, state);
    todo!("part A")
}

/// One issue with its labels, assignees, body, author and state.
pub fn issue<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    number: u64,
) -> Result<IssueDetail, TrackerError> {
    let _ = (gh, repo, number);
    todo!("part A")
}

/// The open issues GitHub's dependency links say block `issue`.
pub fn open_blockers<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    issue: u64,
) -> Result<Vec<Blocker>, TrackerError> {
    let _ = (gh, repo, issue);
    todo!("part A")
}

/// The issues the body's `## Depends on` section names that are not closed.
pub fn text_blockers<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    issue: u64,
) -> Result<Vec<Blocker>, TrackerError> {
    let _ = (gh, repo, issue);
    todo!("part A")
}

/// The pull request; `None` for a number the repo does not have.
pub fn pull_request<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    number: u64,
) -> Result<Option<PullRequest>, TrackerError> {
    let _ = (gh, repo, number);
    todo!("part A")
}

/// The open pull requests from a branch named `head`, forks included.
pub fn open_pull_requests_on_head<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    head: &str,
) -> Result<Vec<OpenPullRequest>, TrackerError> {
    let _ = (gh, repo, head, invalid_argument);
    todo!("part A")
}

/// How each check in `names` stands on the commit `sha`.
pub fn checks<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    sha: &str,
    names: &[String],
) -> Result<Vec<CheckStatus>, TrackerError> {
    let _ = (gh, repo, sha, names);
    todo!("part A")
}

/// The repo's default branch.
pub fn default_branch<G: Tracker + ?Sized>(gh: &G, repo: &str) -> Result<String, TrackerError> {
    let _ = (gh, repo);
    todo!("part A")
}

/// The full commit id at the tip of `branch`.
pub fn branch_tip<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    branch: &str,
) -> Result<String, TrackerError> {
    let _ = (gh, repo, branch);
    todo!("part A")
}

/// The text of the file at `path` in the commit `sha`.
pub fn file_at<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    path: &str,
    sha: &str,
) -> Result<String, TrackerError> {
    let _ = (gh, repo, path, sha);
    todo!("part A")
}

// ---------------------------------------------------------------------------------------------
// End of part A.
// ---------------------------------------------------------------------------------------------

// ---------------------------------------------------------------------------------------------
// Part B: approval history, issue activity and pull request feedback.
// ---------------------------------------------------------------------------------------------

/// An instructor's comment made during a build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comment {
    pub id: String,
    pub author: String,
    pub created_at: String,
    pub body: String,
}

/// An instructor's edit of the issue body: when, by whom, and the whole body as it stands now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BodyEdit {
    pub edited_at: String,
    pub editor: String,
    pub body: String,
}

/// Why an instructor's comment was not forwarded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropReason {
    EditedByOther,
    Unverifiable,
}

/// An instructor's comment that was not forwarded because its text may not be the
/// instructor's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dropped {
    pub id: String,
    pub reason: DropReason,
}

/// What the instructors did on an issue since a moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Activity {
    /// Oldest first.
    pub comments: Vec<Comment>,
    pub body_edit: Option<BodyEdit>,
    pub dropped: Vec<Dropped>,
}

/// Where a piece of feedback was written. `state` is GitHub's review state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeedbackKind {
    Review { state: String },
    PrComment,
    IssueComment,
}

/// A comment a review left on a line of the pull request's diff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineComment {
    pub path: String,
    /// The line it is on, or was on when the diff changed; `None` when GitHub gives neither.
    pub line: Option<u64>,
    pub body: String,
}

/// One piece of instructor feedback: a review of a pull request, or a comment on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Feedback {
    /// `review:<id>`, `pr-comment:<id>` or `issue-comment:<id>`, so the id spaces cannot
    /// collide.
    pub id: String,
    pub author: String,
    pub kind: FeedbackKind,
    /// GitHub's UTC form, e.g. `2026-10-04T10:13:16Z`.
    pub created_at: String,
    /// The review's body or the comment's text; empty when there is none.
    pub body: String,
    /// The comments a review left on lines of the diff, in GitHub's order; empty for any other
    /// kind.
    pub inline: Vec<InlineComment>,
    /// How many inline comments of a review were not read; 0 for any other kind.
    pub inline_unread: u64,
}

/// Whether an instructor applied the `factory:ready` label in force, and every edit and
/// rename since was an instructor's.
pub fn approval<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    issue: u64,
    instructors: &[String],
) -> Result<Approval, TrackerError> {
    let _ = (gh, repo, issue, instructors, issue_query_args, quoted);
    todo!("part B")
}

/// Whether the issue text is as an instructor left it at `since`.
pub fn text_check<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    issue: u64,
    since: &str,
    instructors: &[String],
) -> Result<TextCheck, TrackerError> {
    let _ = (gh, repo, issue, since, instructors);
    todo!("part B")
}

/// Whether anyone edited the body or renamed the title at or after `since`.
pub fn text_edited_since<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    issue: u64,
    since: &str,
) -> Result<bool, TrackerError> {
    let _ = (gh, repo, issue, since);
    todo!("part B")
}

/// What the instructors wrote on the issue after `since`.
pub fn activity_since<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    issue: u64,
    since: &str,
    instructors: &[String],
) -> Result<Activity, TrackerError> {
    let _ = (gh, repo, issue, since, instructors);
    todo!("part B")
}

/// The instructors' feedback on pull request `pr` after `since`, reading back no further than
/// `floor`.
pub fn pr_feedback<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    pr: u64,
    since: &str,
    floor: &str,
    instructors: &[String],
) -> Result<Vec<Feedback>, TrackerError> {
    let _ = (gh, repo, pr, since, floor, instructors);
    todo!("part B")
}

// ---------------------------------------------------------------------------------------------
// End of part B.
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::factory::github::{Answer, FakeTracker};

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| part.to_string()).collect()
    }

    #[test]
    fn pages_split_correctly_around_brackets_and_quotes_inside_strings() {
        let pages = "[{\"id\":1,\"body\":\"] [ } \\\" {\"}]\n[{\"id\":3,\"body\":\"s\"}]";
        assert_eq!(
            split_json_values(pages),
            vec![
                "[{\"id\":1,\"body\":\"] [ } \\\" {\"}]",
                "[{\"id\":3,\"body\":\"s\"}]"
            ]
        );
        #[derive(serde::Deserialize)]
        struct Row {
            id: u64,
        }
        let fake = FakeTracker::answering([Answer::ok(pages)]);
        let rows: Vec<Row> = gh_pages(&fake, &args(&["api", "x", "--paginate"]), "rows").unwrap();
        assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![1, 3]);
    }

    #[test]
    fn empty_paginated_output_is_a_parse_error() {
        #[derive(Debug, serde::Deserialize)]
        struct Row {}
        let fake = FakeTracker::answering([Answer::ok(" \n")]);
        let error = gh_pages::<Row, _>(&fake, &args(&["api", "x"]), "JSON arrays of rows")
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "`gh api x` returned unexpected output: JSON arrays of rows (output was empty)"
        );
    }

    #[test]
    fn a_utc_second_is_exactly_yyyy_mm_dd_thh_mm_ssz() {
        for good in ["2026-10-04T10:13:16Z", "0000-00-00T00:00:00Z"] {
            assert!(is_utc_second(good), "{good}");
        }
        for bad in [
            "2026-10-04T10:13:16.000Z",
            "2026-10-04T10:13:16+00:00",
            "2026-10-04 10:13:16Z",
            "2026-10-04T10:13:16z",
            "2026-1-04T10:13:16Z",
            "",
            "2026-10-04T10:13:1٦Z",
        ] {
            assert!(!is_utc_second(bad), "{bad}");
        }
    }

    #[test]
    fn a_graphql_call_for_a_repo_that_is_not_owner_name_is_a_parse_error() {
        assert_eq!(
            issue_query_args("softfac", "query", 6).unwrap_err().to_string(),
            "`gh api graphql` returned unexpected output: a repo written as owner/name, got `softfac`"
        );
        assert_eq!(
            issue_query_args("rawphp/softfac", "q", 6).unwrap(),
            args(&[
                "api",
                "graphql",
                "-f",
                "query=q",
                "-f",
                "owner=rawphp",
                "-f",
                "name=softfac",
                "-F",
                "number=6"
            ])
        );
    }

    // ----- Part A tests -----

    // ----- End of part A tests -----

    // ----- Part B tests -----

    // ----- End of part B tests -----
}

